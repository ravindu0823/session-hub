//! Port of `src/store/db.ts`: the `Store` over `hub.db`, row types and queries, statement for
//! statement. Status JSON columns are written with `serde_json` in the field order of the TS objects,
//! which gives the same bytes as `JSON.stringify`, so either side reads what the other wrote.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, Type, ValueRef};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, ToSql, named_params, params};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::schema::MIGRATIONS;
use crate::config::{RELAY_NAME, RETENTION_MS};
use crate::ingest::tail::TailState;
use crate::status::{Status, WaitingItem, answered_ids};
use crate::text::is_js_space;

/// Schema version this build writes and reads (`MIGRATIONS.len()`).
const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

/// Messages deleted per write transaction by purge (the `LIMIT` of `PURGE_SQL`).
const PURGE_BATCH: usize = 500;
const PURGE_SQL: &str = "DELETE FROM messages WHERE id IN
         (SELECT id FROM messages WHERE ts < ? LIMIT 500)";

/// Owner and relay messages newer than each live session's status reply (bound: the relay's name).
/// CROSS JOIN fixes the join order: from the few status rows to the messages index by (session_id,
/// ts). Without it SQLite scanned every message on each read of the session list, and the longer
/// reads kept a purge's WAL checkpoint waiting. Public for the query-plan test.
pub const ANSWERS_SQL: &str = "SELECT m.session_id, m.text FROM status st
  CROSS JOIN sessions s CROSS JOIN messages m
  WHERE s.session_id = st.session_id AND s.ended_at IS NULL AND st.waiting_json <> '[]'
    AND m.session_id = st.session_id AND m.ts > st.reply_ts
    AND (m.role = 'owner' OR (m.role = 'peer' AND m.from_name = ?))
  ORDER BY m.session_id, m.ts";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("hub.db has schema version {found}, newer than this build supports ({supported})")]
    NewerSchema { found: i64, supported: i64 },
    #[error("sqlite WAL mode unavailable")]
    WalUnavailable,
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

/// A string union of the TS store: serde and SQL both use the lowercase names.
macro_rules! str_enum {
    ($(#[$doc:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "lowercase")]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }

            pub fn parse(s: &str) -> Option<Self> {
                match s {
                    $($text => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }

        impl ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                Ok(ToSqlOutput::from(self.as_str()))
            }
        }
    };
}

/// Decoding from SQL is strict: an unrecognised string is an error.
macro_rules! strict_from_sql {
    ($name:ident) => {
        impl FromSql for $name {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                Self::parse(value.as_str()?).ok_or_else(|| {
                    FromSqlError::Other(
                        concat!("unrecognised ", stringify!($name), " value").into(),
                    )
                })
            }
        }
    };
}

str_enum!(
    /// A session's live state from `claude agents` (`SessionRow.status` in TS).
    LiveStatus { Busy => "busy", Idle => "idle", Waiting => "waiting", Unknown => "unknown" }
);
/// Like TS `parseStatus`: any other string reads back as `unknown`, so a row from a newer build
/// still lists.
impl FromSql for LiveStatus {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        Ok(Self::parse(value.as_str()?).unwrap_or(Self::Unknown))
    }
}
str_enum!(
    /// Who wrote an indexed message.
    Role { Owner => "owner", Peer => "peer", Assistant => "assistant" }
);
strict_from_sql!(Role);
str_enum!(
    /// State of a relay send.
    SendState {
        Pending => "pending",
        Held => "held",
        Delivered => "delivered",
        Expired => "expired",
        Failed => "failed",
    }
);
strict_from_sql!(SendState);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRow {
    pub session_id: String,
    pub profile: String,
    pub name: String,
    pub cwd: String,
    pub pid: i64,
    pub kind: String,
    pub status: LiveStatus,
    pub started_at: i64,
    pub updated_at: i64,
    pub ended_at: Option<i64>,
    pub transcript_path: String,
    pub permission_mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageRow {
    pub uuid: String,
    pub session_id: String,
    /// ISO UTC with milliseconds, compared as text.
    pub ts: String,
    pub role: Role,
    pub from_name: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendRow {
    /// None for a row not yet created (TS `id?: number`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    pub session_id: String,
    pub profile: String,
    pub name: String,
    pub sha256: String,
    pub created_at: i64,
    pub state: SendState,
    pub detail: Option<String>,
}

/// A waiting item as shown: `answered` when the owner named its ID in the session's terminal, or a
/// Session Hub relay message did, after the status reply (`status::answered_ids`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewWaitingItem {
    #[serde(flatten)]
    pub item: WaitingItem,
    pub answered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredStatus {
    pub progress_pct: Option<i64>,
    pub phase: Option<String>,
    pub remaining: Vec<String>,
    pub waiting: Vec<ViewWaitingItem>,
    pub recommendations: Vec<String>,
    pub reply_uuid: String,
    pub reply_ts: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LatestReply {
    pub reply_uuid: String,
    pub reply_ts: String,
    pub has_status: bool,
}

/// A session as listed. `SessionRow.status` (busy/idle/...) is exposed as `state` because `status`
/// holds the parsed four-part status. Field order is the key order of the TS object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    pub session_id: String,
    pub profile: String,
    pub name: String,
    pub cwd: String,
    pub pid: i64,
    pub kind: String,
    pub started_at: i64,
    pub updated_at: i64,
    pub ended_at: Option<i64>,
    pub transcript_path: String,
    pub permission_mode: Option<String>,
    pub state: LiveStatus,
    pub status: Option<StoredStatus>,
    pub latest_reply: Option<LatestReply>,
    pub waiting_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub session_id: String,
    pub uuid: String,
    pub ts: String,
    pub role: Role,
    pub snippet: String,
}

/// `new Date(ms).toISOString()`: ISO 8601 UTC with milliseconds; years outside 0..=9999 use the
/// six-digit signed form (`+010000`, `-000001`).
pub fn iso_utc_ms(ms: i64) -> String {
    const DAY_MS: i64 = 86_400_000;
    let (days, rem) = (ms.div_euclid(DAY_MS), ms.rem_euclid(DAY_MS));
    let (y, mo, d) = civil_from_days(days);
    let year = if (0..=9999).contains(&y) {
        format!("{y:04}")
    } else if y < 0 {
        format!("-{:06}", -y)
    } else {
        format!("+{y:06}")
    };
    format!(
        "{year}-{mo:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000
    )
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian (H. Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// The FTS5 query for a search box: each whitespace-separated word (JS `\s`) as a quoted phrase,
/// so operators and quotes are literal; words are ANDed.
fn fts_query(q: &str) -> String {
    q.split(is_js_space)
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

fn to_session(r: &Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        session_id: r.get("session_id")?,
        profile: r.get("profile")?,
        name: r.get("name")?,
        cwd: r.get("cwd")?,
        pid: num_col(r, "pid")?,
        kind: r.get("kind")?,
        status: r.get("status")?,
        started_at: num_col(r, "started_at")?,
        updated_at: num_col(r, "updated_at")?,
        ended_at: opt_num_col(r, "ended_at")?,
        transcript_path: r.get("transcript_path")?,
        permission_mode: r.get("permission_mode")?,
    })
}

fn to_message(r: &Row<'_>) -> rusqlite::Result<MessageRow> {
    Ok(MessageRow {
        uuid: r.get("uuid")?,
        session_id: r.get("session_id")?,
        ts: r.get("ts")?,
        role: r.get("role")?,
        from_name: r.get("from_name")?,
        text: r.get("text")?,
    })
}

fn to_send(r: &Row<'_>) -> rusqlite::Result<SendRow> {
    Ok(SendRow {
        id: Some(r.get("id")?),
        session_id: r.get("session_id")?,
        profile: r.get("profile")?,
        name: r.get("name")?,
        sha256: r.get("sha256")?,
        created_at: r.get("created_at")?,
        state: r.get("state")?,
        detail: r.get("detail")?,
    })
}

/// A JSON text column parsed like `JSON.parse`; a parse failure is a column conversion error. The
/// error names only the column: serde's message quotes the rejected value (status text), and store
/// errors reach daemon.log.
fn json_col<T: DeserializeOwned>(r: &Row<'_>, name: &str) -> rusqlite::Result<T> {
    let text: String = r.get(name)?;
    serde_json::from_str(&text).map_err(|_| {
        let idx = r.as_ref().column_index(name).unwrap_or(0);
        rusqlite::Error::FromSqlConversionFailure(
            idx,
            Type::Text,
            format!("invalid JSON in {name}").into(),
        )
    })
}

/// An INTEGER column TS may have stored as REAL (bun:sqlite binds a JS number with a fraction as
/// REAL): a REAL is truncated toward zero, like `progress_pct`. NULL or text is a type error.
fn num_col(r: &Row<'_>, name: &str) -> rusqlite::Result<i64> {
    opt_num_col(r, name)?.ok_or_else(|| {
        let idx = r.as_ref().column_index(name).unwrap_or(0);
        rusqlite::Error::InvalidColumnType(idx, name.to_string(), Type::Null)
    })
}

/// [`num_col`] for a nullable column.
fn opt_num_col(r: &Row<'_>, name: &str) -> rusqlite::Result<Option<i64>> {
    match r.get_ref(name)? {
        ValueRef::Null => Ok(None),
        ValueRef::Integer(i) => Ok(Some(i)),
        // `as` saturates at the i64 bounds; SQLite stores no NaN (it becomes NULL).
        ValueRef::Real(f) => Ok(Some(f as i64)),
        other => {
            let idx = r.as_ref().column_index(name).unwrap_or(0);
            Err(rusqlite::Error::InvalidColumnType(
                idx,
                name.to_string(),
                other.data_type(),
            ))
        }
    }
}

/// A byte offset stored as INTEGER (SQLite integers are i64).
fn u64_col(r: &Row<'_>, name: &str) -> rusqlite::Result<u64> {
    let v: i64 = r.get(name)?;
    u64::try_from(v).map_err(|e| {
        let idx = r.as_ref().column_index(name).unwrap_or(0);
        rusqlite::Error::FromSqlConversionFailure(idx, Type::Integer, Box::new(e))
    })
}

fn sql_i64(v: u64) -> rusqlite::Result<i64> {
    i64::try_from(v).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
}

fn to_json<T: Serialize + ?Sized>(v: &T) -> rusqlite::Result<String> {
    serde_json::to_string(v).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
}

/// The readable sessions, and how many rows were skipped because they could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionList {
    pub sessions: Vec<SessionView>,
    pub unreadable: usize,
}

/// One row of the session list before answered items are marked.
struct ListedRow {
    session: SessionRow,
    /// The stored status with `waiting` still empty: it is filled in once answers are known.
    status: Option<StoredStatus>,
    waiting: Vec<WaitingItem>,
    latest_reply: Option<LatestReply>,
}

/// Rolls an open transaction (or savepoint) back unless disarmed, also when the body panics.
struct Rollback<'a> {
    conn: &'a Connection,
    sql: Option<&'static str>,
}

impl Drop for Rollback<'_> {
    fn drop(&mut self) {
        if let Some(sql) = self.sql {
            let _ = self.conn.execute_batch(sql);
        }
    }
}

/// The index. One connection per `Store`; `Send` but not `Sync`.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens the index file, creating it when missing unless `create` is false (then a missing file
    /// is an error and no file is created). The caller ensures the directory exists.
    pub fn open(path: &Path, create: bool) -> Result<Store, StoreError> {
        let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        if create {
            flags |= OpenFlags::SQLITE_OPEN_CREATE;
        }
        let conn = Connection::open_with_flags(path, flags)?;
        conn.execute_batch("PRAGMA busy_timeout = 5000")?;
        let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        if mode != "wal" {
            return Err(StoreError::WalUnavailable);
        }
        conn.execute_batch("PRAGMA synchronous = NORMAL")?;
        conn.execute_batch("PRAGMA foreign_keys = ON")?;
        // Freed pages are zeroed, so deleted message text cannot be read back from the file (PII-03).
        conn.execute_batch("PRAGMA secure_delete = ON")?;
        let store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    /// Opens an existing index for reading only (the TUI): never creates, migrates or writes. A
    /// newer schema is refused like `open`; the file must already be in WAL mode.
    pub fn open_read_only(path: &Path) -> Result<Store, StoreError> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(path, flags)?;
        conn.execute_batch("PRAGMA busy_timeout = 5000")?;
        let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
        if mode != "wal" {
            return Err(StoreError::WalUnavailable);
        }
        let store = Store { conn };
        let current = store.schema_version()?;
        if current > SCHEMA_VERSION {
            return Err(StoreError::NewerSchema {
                found: current,
                supported: SCHEMA_VERSION,
            });
        }
        Ok(store)
    }

    fn schema_version(&self) -> Result<i64, StoreError> {
        let v: Option<i64> =
            self.conn
                .query_row("SELECT MAX(version) AS v FROM schema_version", [], |r| {
                    r.get(0)
                })?;
        Ok(v.unwrap_or(0))
    }

    fn migrate(&self) -> Result<(), StoreError> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)",
        )?;
        self.immediate(|| {
            let mut current = self.schema_version()?;
            if current > SCHEMA_VERSION {
                return Err(StoreError::NewerSchema {
                    found: current,
                    supported: SCHEMA_VERSION,
                });
            }
            while current < SCHEMA_VERSION {
                self.conn.execute_batch(MIGRATIONS[current as usize])?;
                current += 1;
                self.conn
                    .execute("INSERT INTO schema_version (version) VALUES (?)", [current])?;
            }
            Ok(())
        })
    }

    /// How long a statement waits for another connection's write lock before failing with
    /// `SQLITE_BUSY` (5 s after open). Tests shorten it.
    pub fn set_busy_timeout(&self, d: std::time::Duration) -> Result<(), StoreError> {
        self.conn.busy_timeout(d)?;
        Ok(())
    }

    pub fn close(self) -> Result<(), StoreError> {
        self.conn.close().map_err(|(_, e)| StoreError::Sqlite(e))
    }

    /// Runs `f` in one write transaction (`BEGIN IMMEDIATE`); rolls back if it returns an error.
    /// Nested calls (here and in the store's own write methods) become savepoints, as in Bun.
    pub fn tx<T, E: From<StoreError>>(&self, f: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
        self.immediate(f)
    }

    fn immediate<T, E: From<StoreError>>(&self, f: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
        let sql_err = |e: rusqlite::Error| E::from(StoreError::Sqlite(e));
        let (begin, commit, rollback) = if self.conn.is_autocommit() {
            ("BEGIN IMMEDIATE", "COMMIT", "ROLLBACK")
        } else {
            (
                "SAVEPOINT hub_tx",
                "RELEASE hub_tx",
                "ROLLBACK TO hub_tx; RELEASE hub_tx",
            )
        };
        self.conn.execute_batch(begin).map_err(sql_err)?;
        let mut guard = Rollback {
            conn: &self.conn,
            sql: Some(rollback),
        };
        let out = f()?;
        self.conn.execute_batch(commit).map_err(sql_err)?;
        guard.sql = None;
        Ok(out)
    }

    /// VACUUM, then a truncating checkpoint so the WAL file keeps no copy of purged pages.
    pub fn vacuum(&self) -> Result<(), StoreError> {
        self.conn.execute_batch("VACUUM")?;
        self.checkpoint()
    }

    fn checkpoint(&self) -> Result<(), StoreError> {
        self.conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(())
    }

    // ---- sessions ----

    pub fn upsert_session(&self, s: &SessionRow) -> Result<(), StoreError> {
        self.conn
            .prepare_cached(
                "INSERT INTO sessions (session_id, profile, name, cwd, pid, kind, status, started_at,
           updated_at, ended_at, transcript_path, permission_mode)
         VALUES ($sessionId, $profile, $name, $cwd, $pid, $kind, $status, $startedAt,
           $updatedAt, $endedAt, $transcriptPath, $permissionMode)
         ON CONFLICT(session_id) DO UPDATE SET
           profile=excluded.profile, name=excluded.name, cwd=excluded.cwd, pid=excluded.pid,
           kind=excluded.kind, status=excluded.status, started_at=excluded.started_at,
           updated_at=excluded.updated_at, ended_at=excluded.ended_at,
           transcript_path=excluded.transcript_path, permission_mode=excluded.permission_mode",
            )?
            .execute(named_params! {
                "$sessionId": s.session_id,
                "$profile": s.profile,
                "$name": s.name,
                "$cwd": s.cwd,
                "$pid": s.pid,
                "$kind": s.kind,
                "$status": s.status,
                "$startedAt": s.started_at,
                "$updatedAt": s.updated_at,
                "$endedAt": s.ended_at,
                "$transcriptPath": s.transcript_path,
                "$permissionMode": s.permission_mode,
            })?;
        Ok(())
    }

    pub fn mark_ended(&self, session_id: &str, at: i64) -> Result<(), StoreError> {
        self.conn
            .prepare_cached(
                "UPDATE sessions SET ended_at = ?, updated_at = ? WHERE session_id = ?",
            )?
            .execute(params![at, at, session_id])?;
        Ok(())
    }

    /// One session row by id, or None.
    pub fn get_session(&self, session_id: &str) -> Result<Option<SessionRow>, StoreError> {
        Ok(self
            .conn
            .prepare_cached("SELECT * FROM sessions WHERE session_id = ?")?
            .query_row([session_id], to_session)
            .optional()?)
    }

    /// The session list without the rows that could not be decoded (see [`Self::list_sessions_counted`]).
    pub fn list_sessions(&self) -> Result<Vec<SessionView>, StoreError> {
        Ok(self.list_sessions_counted()?.sessions)
    }

    /// The session list. A row that cannot be decoded (corrupt status JSON, a wrong column type) is
    /// skipped and counted in `unreadable`, so one bad row does not hide every session; the caller
    /// reports the count, never the row's values. A query error (busy, missing table) still fails.
    pub fn list_sessions_counted(&self) -> Result<SessionList, StoreError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT s.*, st.progress_pct, st.phase, st.remaining_json, st.waiting_json,
                st.recommendations_json, st.reply_uuid AS st_reply_uuid, st.reply_ts AS st_reply_ts,
                lr.reply_uuid AS lr_reply_uuid, lr.reply_ts AS lr_reply_ts, lr.has_status
           FROM sessions s
           LEFT JOIN status st ON st.session_id = s.session_id
           LEFT JOIN latest_reply lr ON lr.session_id = s.session_id
          ORDER BY (s.ended_at IS NOT NULL), s.profile, s.name, s.session_id",
        )?;
        let decode = |r: &Row<'_>| -> rusqlite::Result<ListedRow> {
            // JS truthiness: a NULL or empty waiting_json lists nothing.
            let waiting_json: Option<String> = r.get("waiting_json")?;
            let waiting = match waiting_json.as_deref() {
                Some(t) if !t.is_empty() => json_col(r, "waiting_json")?,
                _ => Vec::new(),
            };
            let st_reply_uuid: Option<String> = r.get("st_reply_uuid")?;
            let status = match st_reply_uuid {
                None => None,
                Some(reply_uuid) => {
                    // REAL column: TS writes a whole number, so the value is exact.
                    let pct: Option<f64> = r.get("progress_pct")?;
                    Some(StoredStatus {
                        progress_pct: pct.map(|p| p as i64),
                        phase: r.get("phase")?,
                        remaining: json_col(r, "remaining_json")?,
                        waiting: Vec::new(),
                        recommendations: json_col(r, "recommendations_json")?,
                        reply_uuid,
                        reply_ts: r.get("st_reply_ts")?,
                    })
                }
            };
            let lr_reply_uuid: Option<String> = r.get("lr_reply_uuid")?;
            let latest_reply = match lr_reply_uuid {
                None => None,
                Some(reply_uuid) => Some(LatestReply {
                    reply_uuid,
                    reply_ts: r.get("lr_reply_ts")?,
                    has_status: r.get::<_, Option<i64>>("has_status")? == Some(1),
                }),
            };
            Ok(ListedRow {
                session: to_session(r)?,
                status,
                waiting,
                latest_reply,
            })
        };
        let mut rows: Vec<ListedRow> = Vec::new();
        let mut unreadable = 0;
        let mut cursor = stmt.query([])?;
        while let Some(r) = cursor.next()? {
            match decode(r) {
                Ok(row) => rows.push(row),
                Err(_) => unreadable += 1,
            }
        }
        let answers = self.answers_since_status()?;
        let sessions = rows
            .into_iter()
            .map(|r| {
                let listed: Vec<String> = r.waiting.iter().filter_map(|w| w.id.clone()).collect();
                let texts = answers
                    .get(&r.session.session_id)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let answered = answered_ids(&listed, texts);
                let waiting: Vec<ViewWaitingItem> = r
                    .waiting
                    .into_iter()
                    .map(|item| {
                        let answered = item.id.as_ref().is_some_and(|id| answered.contains(id));
                        ViewWaitingItem { item, answered }
                    })
                    .collect();
                let waiting_count = waiting.iter().filter(|w| !w.answered).count() as i64;
                let status = r.status.map(|st| StoredStatus { waiting, ..st });
                let s = r.session;
                SessionView {
                    session_id: s.session_id,
                    profile: s.profile,
                    name: s.name,
                    cwd: s.cwd,
                    pid: s.pid,
                    kind: s.kind,
                    started_at: s.started_at,
                    updated_at: s.updated_at,
                    ended_at: s.ended_at,
                    transcript_path: s.transcript_path,
                    permission_mode: s.permission_mode,
                    state: s.status,
                    status,
                    latest_reply: r.latest_reply,
                    waiting_count,
                }
            })
            .collect();
        Ok(SessionList {
            sessions,
            unreadable,
        })
    }

    /// Per live session with waiting items, the texts of the owner's own messages and of relay
    /// messages newer than its status reply, oldest first: what can answer that reply's waiting
    /// items. Ended sessions are not counted anywhere, so their items are never marked.
    fn answers_since_status(&self) -> Result<HashMap<String, Vec<String>>, StoreError> {
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        let mut stmt = self.conn.prepare_cached(ANSWERS_SQL)?;
        let rows = stmt.query_map([RELAY_NAME], |r| {
            Ok((
                r.get::<_, String>("session_id")?,
                r.get::<_, String>("text")?,
            ))
        })?;
        for row in rows {
            let (session_id, text) = row?;
            out.entry(session_id).or_default().push(text);
        }
        Ok(out)
    }

    // ---- tails ----

    pub fn get_tail(&self, path: &str) -> Result<Option<TailState>, StoreError> {
        Ok(self
            .conn
            .prepare_cached("SELECT * FROM tails WHERE path = ?")?
            .query_row([path], |r| {
                Ok(TailState {
                    path: r.get("path")?,
                    offset: u64_col(r, "offset")?,
                    size: u64_col(r, "size")?,
                    first_uuid: r.get("first_uuid")?,
                })
            })
            .optional()?)
    }

    pub fn save_tail(&self, t: &TailState) -> Result<(), StoreError> {
        self.conn
            .prepare_cached(
                "INSERT INTO tails (path, offset, size, first_uuid) VALUES (?, ?, ?, ?)
         ON CONFLICT(path) DO UPDATE SET offset=excluded.offset, size=excluded.size,
           first_uuid=excluded.first_uuid",
            )?
            .execute(params![
                t.path,
                sql_i64(t.offset)?,
                sql_i64(t.size)?,
                t.first_uuid
            ])?;
        Ok(())
    }

    // ---- messages ----

    /// Text must already be masked by the caller. Duplicate uuids are ignored. `session_id` is
    /// stored for every row (the rows' own `session_id` is not read), as in TS.
    pub fn add_messages(&self, session_id: &str, rows: &[MessageRow]) -> Result<(), StoreError> {
        self.immediate(|| {
            let mut ins = self.conn.prepare_cached(
                "INSERT OR IGNORE INTO messages (uuid, session_id, ts, role, from_name, text)
       VALUES (?, ?, ?, ?, ?, ?)",
            )?;
            for m in rows {
                ins.execute(params![
                    m.uuid,
                    session_id,
                    m.ts,
                    m.role,
                    m.from_name,
                    m.text
                ])?;
            }
            Ok(())
        })
    }

    /// One page, oldest first, before the cursor. Pages run in `ts DESC, id DESC` order. With a
    /// `(ts, uuid)` cursor, rows of the same ts that come before that row in the page order are
    /// included, so no row is skipped or repeated across pages. An unknown uuid acts like a ts-only
    /// cursor (rows sharing that ts are skipped), which is TS's `history(id, limit, beforeTs)`.
    pub fn history(
        &self,
        session_id: &str,
        limit: i64,
        before: Option<(&str, &str)>,
    ) -> Result<Vec<MessageRow>, StoreError> {
        let mut rows: Vec<MessageRow> = match before {
            None => self
                .conn
                .prepare_cached(
                    "SELECT * FROM messages WHERE session_id = ? ORDER BY ts DESC, id DESC LIMIT ?",
                )?
                .query_map(params![session_id, limit], to_message)?
                .collect::<rusqlite::Result<_>>()?,
            Some((ts, uuid)) => self
                .conn
                .prepare_cached(
                    "SELECT * FROM messages WHERE session_id = $sid
              AND (ts < $ts OR (ts = $ts AND id < (
                SELECT id FROM messages WHERE session_id = $sid AND uuid = $uuid)))
            ORDER BY ts DESC, id DESC LIMIT $limit",
                )?
                .query_map(
                    named_params! { "$sid": session_id, "$ts": ts, "$uuid": uuid, "$limit": limit },
                    to_message,
                )?
                .collect::<rusqlite::Result<_>>()?,
        };
        rows.reverse();
        Ok(rows)
    }

    pub fn search(&self, q: &str, limit: i64) -> Result<Vec<SearchHit>, StoreError> {
        let query = fts_query(q);
        if query.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self
            .conn
            .prepare_cached(
                "SELECT m.session_id, m.uuid, m.ts, m.role,
                snippet(messages_fts, 0, '[', ']', '...', 12) AS snippet
           FROM messages_fts
           JOIN messages m ON m.id = messages_fts.rowid
          WHERE messages_fts MATCH ?
          ORDER BY m.ts DESC, m.id DESC
          LIMIT ?",
            )?
            .query_map(params![query, limit], |r| {
                Ok(SearchHit {
                    session_id: r.get("session_id")?,
                    uuid: r.get("uuid")?,
                    ts: r.get("ts")?,
                    role: r.get("role")?,
                    snippet: r.get("snippet")?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    // ---- status ----

    pub fn set_status(
        &self,
        session_id: &str,
        st: &Status,
        reply_uuid: &str,
        reply_ts: &str,
    ) -> Result<(), StoreError> {
        self.conn
            .prepare_cached(
                "INSERT INTO status (session_id, progress_pct, phase, remaining_json, waiting_json,
           recommendations_json, reply_uuid, reply_ts) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(session_id) DO UPDATE SET progress_pct=excluded.progress_pct,
           phase=excluded.phase, remaining_json=excluded.remaining_json,
           waiting_json=excluded.waiting_json, recommendations_json=excluded.recommendations_json,
           reply_uuid=excluded.reply_uuid, reply_ts=excluded.reply_ts",
            )?
            .execute(params![
                session_id,
                st.progress_pct,
                st.phase,
                to_json(&st.remaining)?,
                to_json(&st.waiting)?,
                to_json(&st.recommendations)?,
                reply_uuid,
                reply_ts,
            ])?;
        Ok(())
    }

    pub fn set_latest_reply(
        &self,
        session_id: &str,
        reply_uuid: &str,
        reply_ts: &str,
        has_status: bool,
    ) -> Result<(), StoreError> {
        self.conn
            .prepare_cached(
                "INSERT INTO latest_reply (session_id, reply_uuid, reply_ts, has_status) VALUES (?, ?, ?, ?)
         ON CONFLICT(session_id) DO UPDATE SET reply_uuid=excluded.reply_uuid,
           reply_ts=excluded.reply_ts, has_status=excluded.has_status",
            )?
            .execute(params![session_id, reply_uuid, reply_ts, i64::from(has_status)])?;
        Ok(())
    }

    // ---- retention ----

    /// Deletes sessions ended before now - RETENTION_MS (cascading), their tails, old sends, and
    /// messages older than the window in every session, live ones included (PII-03). Message
    /// timestamps are ISO UTC strings, so they compare as text. Returns the sessions deleted.
    pub fn purge(&self, now: i64) -> Result<usize, StoreError> {
        let cutoff = now - RETENTION_MS as i64;
        let cutoff_iso = iso_utc_ms(cutoff);
        // Old messages go in batches, each its own short write transaction: the first purge after an
        // upgrade can hold days of a live session's backlog, and one huge delete (with secure-delete)
        // would hold the write lock past other processes' 5 s busy timeout.
        loop {
            let n = self.immediate(|| -> Result<usize, StoreError> {
                Ok(self
                    .conn
                    .prepare_cached(PURGE_SQL)?
                    .execute([&cutoff_iso])?)
            })?;
            if n < PURGE_BATCH {
                break;
            }
        }
        let deleted = self.immediate(|| -> Result<i64, StoreError> {
            // Parsed status text of a reply older than the window goes with the messages.
            self.conn
                .execute("DELETE FROM status WHERE reply_ts < ?", [&cutoff_iso])?;
            self.conn
                .execute("DELETE FROM latest_reply WHERE reply_ts < ?", [&cutoff_iso])?;
            self.conn.execute(
                "DELETE FROM tails WHERE path IN
               (SELECT transcript_path FROM sessions WHERE ended_at IS NOT NULL AND ended_at < ?)
             AND path NOT IN
               (SELECT transcript_path FROM sessions WHERE ended_at IS NULL OR ended_at >= ?)",
                [cutoff, cutoff],
            )?;
            let n: i64 = self.conn.query_row(
                "SELECT COUNT(*) AS n FROM sessions WHERE ended_at IS NOT NULL AND ended_at < ?",
                [cutoff],
                |r| r.get(0),
            )?;
            self.conn.execute(
                "DELETE FROM sessions WHERE ended_at IS NOT NULL AND ended_at < ?",
                [cutoff],
            )?;
            self.conn
                .execute("DELETE FROM sends WHERE created_at < ?", [cutoff])?;
            Ok(n)
        })?;
        // The deletes zero the freed pages (secure_delete), but older WAL frames still hold the text
        // until a checkpoint: move them into the database and truncate the WAL now, not at the daily
        // vacuum (PII-03). Best effort: with a reader open it checkpoints what it can.
        self.checkpoint()?;
        Ok(usize::try_from(deleted).unwrap_or(0))
    }

    // ---- sends ----

    pub fn create_send(&self, row: &SendRow) -> Result<i64, StoreError> {
        self.conn
            .prepare_cached(
                "INSERT INTO sends (session_id, profile, name, sha256, created_at, state, detail)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
            )?
            .execute(params![
                row.session_id,
                row.profile,
                row.name,
                row.sha256,
                row.created_at,
                row.state,
                row.detail,
            ])?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Conditional state change: from pending/held to any state, from failed only to delivered
    /// (the relay reported failure but the message arrived). delivered and expired never change.
    /// `detail` None keeps the stored detail, except that failed -> delivered clears it: the
    /// relay's failure reason no longer applies. `Some(d)` stores `d`. Returns whether a row changed.
    pub fn transition_send(
        &self,
        id: i64,
        to: SendState,
        detail: Option<Option<&str>>,
    ) -> Result<bool, StoreError> {
        let changed = match detail {
            None => self
                .conn
                .prepare_cached(
                    "UPDATE sends SET state = $to,
                 detail = CASE WHEN state = 'failed' THEN NULL ELSE detail END
               WHERE id = $id AND (state IN ('pending', 'held') OR (state = 'failed' AND $to = 'delivered'))",
                )?
                .execute(named_params! { "$id": id, "$to": to })?,
            Some(detail) => self
                .conn
                .prepare_cached(
                    "UPDATE sends SET state = $to, detail = $detail WHERE id = $id AND (state IN ('pending', 'held') OR (state = 'failed' AND $to = 'delivered'))",
                )?
                .execute(named_params! { "$id": id, "$to": to, "$detail": detail })?,
        };
        Ok(changed > 0)
    }

    /// Sends created at or after `since` (epoch ms), in every state and session.
    pub fn count_sends_since(&self, since: i64) -> Result<i64, StoreError> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) AS n FROM sends WHERE created_at >= ?",
            [since],
            |r| r.get(0),
        )?)
    }

    pub fn get_send(&self, id: i64) -> Result<Option<SendRow>, StoreError> {
        Ok(self
            .conn
            .prepare_cached("SELECT * FROM sends WHERE id = ?")?
            .query_row([id], to_send)
            .optional()?)
    }

    pub fn open_send_for(&self, session_id: &str) -> Result<Option<SendRow>, StoreError> {
        Ok(self
            .conn
            .prepare_cached(
                "SELECT * FROM sends WHERE session_id = ? AND state IN ('pending', 'held')
          ORDER BY id DESC LIMIT 1",
            )?
            .query_row([session_id], to_send)
            .optional()?)
    }

    /// Every pending/held send, oldest first; only one session's when `session_id` is given.
    pub fn open_sends(&self, session_id: Option<&str>) -> Result<Vec<SendRow>, StoreError> {
        Ok(match session_id {
            None => self
                .conn
                .prepare_cached(
                    "SELECT * FROM sends WHERE state IN ('pending', 'held') ORDER BY id",
                )?
                .query_map([], to_send)?
                .collect::<rusqlite::Result<_>>()?,
            Some(id) => self
                .conn
                .prepare_cached(
                    "SELECT * FROM sends WHERE session_id = ? AND state IN ('pending', 'held')
                ORDER BY id",
                )?
                .query_map([id], to_send)?
                .collect::<rusqlite::Result<_>>()?,
        })
    }

    /// SHA-256 of every send made to this session since `since` (epoch ms), in any state.
    pub fn send_hashes_since(
        &self,
        session_id: &str,
        since: i64,
    ) -> Result<BTreeSet<String>, StoreError> {
        Ok(self
            .conn
            .prepare_cached("SELECT sha256 FROM sends WHERE session_id = ? AND created_at >= ?")?
            .query_map(params![session_id, since], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// One session's sends that transcript evidence can still change, oldest first: pending/held,
    /// plus failed ones created at or after `failed_since` (they may still prove delivered).
    pub fn sends_to_verify(
        &self,
        session_id: &str,
        failed_since: i64,
    ) -> Result<Vec<SendRow>, StoreError> {
        Ok(self
            .conn
            .prepare_cached(
                "SELECT * FROM sends WHERE session_id = ?
           AND (state IN ('pending', 'held') OR (state = 'failed' AND created_at >= ?))
         ORDER BY id",
            )?
            .query_map(params![session_id, failed_since], to_send)?
            .collect::<rusqlite::Result<_>>()?)
    }

    // ---- meta ----

    pub fn set_meta(&self, key: &str, value: Option<&str>) -> Result<(), StoreError> {
        match value {
            None => {
                self.conn
                    .prepare_cached("DELETE FROM meta WHERE key = ?")?
                    .execute([key])?;
            }
            Some(value) => {
                self.conn
                    .prepare_cached(
                        "INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                    )?
                    .execute([key, value])?;
            }
        }
        Ok(())
    }

    /// Changes whenever another connection commits to the file; own writes leave it unchanged.
    pub fn data_version(&self) -> Result<i64, StoreError> {
        Ok(self
            .conn
            .query_row("PRAGMA data_version", [], |r| r.get(0))?)
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .conn
            .prepare_cached("SELECT value FROM meta WHERE key = ?")?
            .query_row([key], |r| r.get(0))
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pragma(store: &Store, name: &str) -> String {
        store
            .conn
            .query_row(&format!("PRAGMA {name}"), [], |r| {
                r.get::<_, rusqlite::types::Value>(0)
            })
            .map(|v| match v {
                rusqlite::types::Value::Integer(i) => i.to_string(),
                rusqlite::types::Value::Text(t) => t,
                other => format!("{other:?}"),
            })
            .unwrap()
    }

    #[test]
    fn pragmas() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.db");
        let s = Store::open(&path, true).unwrap();
        assert_eq!(pragma(&s, "journal_mode"), "wal");
        assert_eq!(pragma(&s, "synchronous"), "1");
        assert_eq!(pragma(&s, "foreign_keys"), "1");
        assert_eq!(pragma(&s, "secure_delete"), "1");
        assert_eq!(pragma(&s, "busy_timeout"), "5000");
        let r = Store::open_read_only(&path).unwrap();
        assert_eq!(pragma(&r, "busy_timeout"), "5000");
        assert_eq!(pragma(&r, "journal_mode"), "wal");
    }

    #[test]
    fn purge_batch_is_the_sql_limit() {
        assert!(PURGE_SQL.ends_with(&format!("LIMIT {PURGE_BATCH})")));
        assert_eq!(SCHEMA_VERSION, 5);
    }

    #[test]
    fn fts_query_quotes_each_js_whitespace_word() {
        assert_eq!(fts_query("  a\u{3000}b\"c\u{feff} "), "\"a\" \"b\"\"c\"");
        assert_eq!(fts_query("\u{85}"), "\"\u{85}\"");
        assert_eq!(fts_query(" \t\n"), "");
    }

    #[test]
    fn unrecognised_enum_text_is_a_column_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.db");
        let s = Store::open(&path, true).unwrap();
        s.conn
            .execute_batch(
                "INSERT INTO sends (session_id, profile, name, sha256, created_at, state, detail)
                 VALUES ('s', 'p', 'n', 'h', 1, 'bogus', NULL)",
            )
            .unwrap();
        let err = s.get_send(1).err().unwrap();
        assert!(
            err.to_string().contains("unrecognised SendState value"),
            "{err}"
        );
    }

    #[test]
    fn corrupt_status_json_is_a_column_error_naming_only_the_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.db");
        let s = Store::open(&path, true).unwrap();
        for (json, value) in [
            (r#"["leaked-text-123"]"#, "leaked-text-123"),
            (r#"[{"id":"Q1","text":7}]"#, "7"),
            ("{", "{"),
        ] {
            let err = s
                .conn
                .query_row("SELECT ?1 AS waiting_json", [json], |r| {
                    json_col::<Vec<WaitingItem>>(r, "waiting_json")
                })
                .unwrap_err();
            assert!(
                matches!(
                    err,
                    rusqlite::Error::FromSqlConversionFailure(0, Type::Text, _)
                ),
                "{err:?}"
            );
            let text = format!("{err} {err:?}");
            assert!(text.contains("invalid JSON in waiting_json"), "{text}");
            if value.len() > 1 {
                assert!(!text.contains(value), "{text}");
            }
        }
    }

    #[test]
    fn a_text_number_column_is_a_type_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.db");
        let s = Store::open(&path, true).unwrap();
        let err = s
            .conn
            .query_row("SELECT 'abc' AS pid, NULL AS ended", [], |r| {
                num_col(r, "pid")
            })
            .unwrap_err();
        assert!(
            matches!(err, rusqlite::Error::InvalidColumnType(0, _, Type::Text)),
            "{err:?}"
        );
        let err = s
            .conn
            .query_row("SELECT NULL AS pid", [], |r| num_col(r, "pid"))
            .unwrap_err();
        assert!(
            matches!(err, rusqlite::Error::InvalidColumnType(0, _, Type::Null)),
            "{err:?}"
        );
    }

    #[test]
    fn tx_rolls_back_when_the_body_panics() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.db");
        let s = Store::open(&path, true).unwrap();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = s.tx(|| -> Result<(), StoreError> {
                s.set_meta("k", Some("v"))?;
                panic!("boom");
            });
        }));
        assert!(r.is_err());
        assert!(s.conn.is_autocommit());
        assert_eq!(s.get_meta("k").unwrap(), None);
    }
}
