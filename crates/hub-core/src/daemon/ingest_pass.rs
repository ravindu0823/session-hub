//! One ingest pass over a transcript: reads new complete lines from the stored offset, classifies
//! them, masks and stores messages, extracts the status of the newest reply, and saves the new
//! offset, one SQLite transaction per batch. Port of `src/daemon/ingest-pass.ts`.

use std::error::Error;
use std::fmt;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use serde_json::Value;

use crate::config::FIRST_INDEX_BYTES;
use crate::ingest::records::{Classified, classify, replace_lone_surrogates};
use crate::ingest::tail::{TailState, initial_state, read_new};
use crate::security::mask_secrets;
use crate::send::parse_js_date;
use crate::status::{Status, WaitingItem, extract_status};
use crate::store::{MessageRow, Role, SessionRow, Store, StoreError, iso_utc_ms};
use crate::text::is_js_space;

pub const READ_CHUNK_BYTES: u64 = 4 * 1024 * 1024;
/// Floor per read, so a long line is never crawled in tiny chunks near the budget's end.
pub const MIN_READ_BYTES: u64 = 64 * 1024;
pub const MAX_BYTES_PER_PASS: u64 = 8 * 1024 * 1024;
pub const MAX_UNRECOGNIZED_TYPES: usize = 10;
/// Number.MAX_SAFE_INTEGER, the largest stored count read back.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// The per-batch records hook (in the daemon, the collector's wrapper of `send::delivery_checker`).
pub type OnRecords<'a> = &'a mut dyn FnMut(&Store, &str, &[Classified]);

pub struct IngestCtx<'a> {
    pub store: &'a Store,
    /// Cached session row (the daemon is the only writer of sessions).
    pub get_row: &'a dyn Fn(&str) -> Option<SessionRow>,
    /// Called after a committed batch changed the row (permission mode, updated time).
    pub row_changed: &'a dyn Fn(&SessionRow),
    /// Clock for `updated_at`, epoch ms.
    pub now: &'a dyn Fn() -> i64,
    /// Handed every committed batch's records (the delivery checker).
    pub on_records: Option<OnRecords<'a>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IngestResult {
    pub lines: usize,
    pub messages: usize,
    pub unknown: usize,
    /// Records of a type this build does not know (diagnostics only, never the banner).
    pub unrecognized: usize,
    /// Byte budget hit: more complete lines may be waiting.
    pub more: bool,
}

/// Passed to `on_records_error` when the `on_records` hook panicked. It names no record and never
/// carries the panic text, which could quote one.
#[derive(Debug)]
pub struct OnRecordsPanicked;

impl fmt::Display for OnRecordsPanicked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("on_records panicked")
    }
}

impl Error for OnRecordsPanicked {}

/// Masks every string in a status; numbers and flags are kept.
pub fn mask_status(st: &Status) -> Status {
    let all = |v: &[String]| v.iter().map(|s| mask_secrets(s)).collect::<Vec<_>>();
    Status {
        progress_pct: st.progress_pct,
        phase: st.phase.as_deref().map(mask_secrets),
        remaining: all(&st.remaining),
        waiting: st
            .waiting
            .iter()
            .map(|w| WaitingItem {
                id: w.id.as_deref().map(mask_secrets),
                text: mask_secrets(&w.text),
                is_new: w.is_new,
                children: all(&w.children),
            })
            .collect(),
        recommendations: all(&st.recommendations),
    }
}

/// `^[\w.<extra>]{1,max}$` with JS's ASCII `\w`.
fn plain_name(s: &str, extra: &[u8], max: usize) -> bool {
    (1..=max).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || extra.contains(&b))
}

/// Record type names kept for diagnostics: short, plain names only (`/^[\w.-]{1,40}$/`).
fn is_type_name(s: &str) -> bool {
    plain_name(s, b".-", 40)
}

/// The record's version is untrusted text: stored only when it looks like a version
/// (`/^[\w.+-]{1,32}$/`), else `"invalid"`.
pub fn safe_version(v: &str) -> String {
    if plain_name(v, b".+-", 32) {
        v.to_string()
    } else {
        "invalid".to_string()
    }
}

struct Newest {
    uuid: String,
    ts: String,
    text: String,
}

#[derive(Default)]
struct Batch {
    records: Vec<Classified>,
    messages: Vec<MessageRow>,
    mode: Option<String>,
    newest: Option<Newest>,
    unknown: usize,
    /// Version of the newest unknown record that had one (`""` when none did).
    unknown_version: String,
    unrecognized: usize,
    /// Distinct safe names of unrecognised types, in first-seen order.
    unrecognized_types: Vec<String>,
}

fn message(
    session_id: &str,
    uuid: &str,
    ts: &str,
    role: Role,
    from: Option<String>,
    text: &str,
) -> MessageRow {
    MessageRow {
        uuid: uuid.to_string(),
        session_id: session_id.to_string(),
        ts: ts.to_string(),
        role,
        from_name: from,
        text: mask_secrets(text),
    }
}

fn collect(session_id: &str, lines: &[String]) -> Batch {
    let mut b = Batch::default();
    for line in lines {
        let r = classify(line);
        match &r {
            Classified::OwnerPrompt { uuid, ts, text } if !uuid.is_empty() => {
                b.messages
                    .push(message(session_id, uuid, ts, Role::Owner, None, text));
            }
            Classified::PeerMessage {
                uuid,
                ts,
                from_name,
                body,
                ..
            } if !uuid.is_empty() => {
                let from = Some(mask_secrets(from_name));
                b.messages
                    .push(message(session_id, uuid, ts, Role::Peer, from, body));
            }
            Classified::AssistantText { uuid, ts, text } if !uuid.is_empty() => {
                b.messages
                    .push(message(session_id, uuid, ts, Role::Assistant, None, text));
                b.newest = Some(Newest {
                    uuid: uuid.clone(),
                    ts: ts.clone(),
                    text: text.clone(),
                });
            }
            Classified::PermissionMode { mode, .. } => b.mode = Some(mode.clone()),
            Classified::Unknown { version, .. } => {
                b.unknown += 1;
                if let Some(v) = version.as_deref().filter(|v| !v.is_empty()) {
                    b.unknown_version = v.to_string();
                }
            }
            Classified::Ignored {
                reason,
                record_type,
            } if reason == "unrecognized-type" => {
                b.unrecognized += 1;
                let t = record_type.as_deref().unwrap_or("");
                if is_type_name(t) && !b.unrecognized_types.iter().any(|x| x == t) {
                    b.unrecognized_types.push(t.to_string());
                }
            }
            _ => {}
        }
        b.records.push(r);
    }
    b
}

/// A stored count read like `Number(meta ?? "0")`: a non-negative safe integer, else 0. (JS also
/// reads `0x`/`0o`/`0b` prefixes; only this file writes these counts, in decimal.)
fn meta_count(store: &Store, key: &str) -> Result<u64, StoreError> {
    let v = store.get_meta(key)?.unwrap_or_else(|| "0".to_string());
    let t = v.trim_matches(is_js_space);
    if t.is_empty() {
        return Ok(0);
    }
    let n = match t.parse::<f64>() {
        // Rust reads "inf"/"nan" words JS does not; neither is a safe integer, so both give 0.
        Ok(n) if n.is_finite() && n.fract() == 0.0 && (0.0..=MAX_SAFE_INTEGER).contains(&n) => n,
        _ => 0.0,
    };
    Ok(n as u64)
}

/// Stored list merged with the batch's names: safe names only, at most [`MAX_UNRECOGNIZED_TYPES`].
fn merge_types(stored: Option<&str>, add: &[String]) -> Vec<String> {
    let prev: Vec<Value> =
        match serde_json::from_str::<Value>(&replace_lone_surrogates(stored.unwrap_or("[]"))) {
            Ok(Value::Array(a)) => a,
            _ => Vec::new(), // corrupt: start over
        };
    let mut out: Vec<String> = Vec::new();
    let names = prev
        .iter()
        .filter_map(|v| v.as_str())
        .chain(add.iter().map(String::as_str));
    for t in names {
        if out.len() >= MAX_UNRECOGNIZED_TYPES {
            break;
        }
        if is_type_name(t) && !out.iter().any(|x| x == t) {
            out.push(t.to_string());
        }
    }
    out
}

/// When the batch's messages happened: the newest message timestamp, capped at `now` (a clock ahead
/// of ours never dates a session in the future). `now` when no timestamp parses.
fn batch_time(messages: &[MessageRow], now: i64) -> i64 {
    messages
        .iter()
        .filter_map(|m| parse_js_date(&m.ts))
        .max()
        .map_or(now, |newest| newest.min(now))
}

/// Stored timestamps are ISO UTC with milliseconds, because history order, paging and retention
/// compare them as text (DATA-07). A missing or unparseable one takes the batch time.
fn normalize_times(messages: &mut [MessageRow], now: i64) {
    let fallback = iso_utc_ms(batch_time(messages, now));
    for m in messages {
        m.ts = match parse_js_date(&m.ts) {
            Some(t) => iso_utc_ms(t),
            None => fallback.clone(),
        };
    }
}

/// Writes one batch and its tail position in a single transaction.
fn commit(
    ctx: &IngestCtx<'_>,
    session_id: &str,
    b: &mut Batch,
    next: &TailState,
) -> Result<(), StoreError> {
    let store = ctx.store;
    let now = (ctx.now)();
    normalize_times(&mut b.messages, now);
    // The status row is dated by its reply, in the same ISO form, so retention can compare it.
    if let Some(n) = b.newest.as_mut()
        && let Some(m) = b.messages.iter().find(|m| m.uuid == n.uuid)
    {
        n.ts = m.ts.clone();
    }
    // Status is parsed from the unmasked text; every stored string is masked.
    let status = b
        .newest
        .as_ref()
        .and_then(|n| extract_status(&n.text))
        .map(|raw| mask_status(&raw));
    let changed = (ctx.get_row)(session_id).and_then(|row| {
        let mode_changed = b
            .mode
            .as_deref()
            .is_some_and(|m| row.permission_mode.as_deref() != Some(m));
        // updated_at means "last time this session's data changed": any committed message (owner,
        // peer or assistant) is such a change, dated by the message itself. A replayed or
        // backfilled batch of old messages therefore never marks a session as just updated, and
        // updated_at never moves back.
        let updated_at = if b.messages.is_empty() {
            row.updated_at
        } else {
            row.updated_at.max(batch_time(&b.messages, now))
        };
        (mode_changed || updated_at != row.updated_at).then(|| SessionRow {
            permission_mode: if mode_changed {
                b.mode.clone()
            } else {
                row.permission_mode.clone()
            },
            updated_at,
            ..row
        })
    });
    store.tx(|| -> Result<(), StoreError> {
        if !b.messages.is_empty() {
            store.add_messages(session_id, &b.messages)?;
        }
        if let Some(row) = &changed {
            store.upsert_session(row)?;
        }
        if let Some(n) = &b.newest {
            if let Some(st) = &status {
                store.set_status(session_id, st, &n.uuid, &n.ts)?;
            }
            store.set_latest_reply(session_id, &n.uuid, &n.ts, status.is_some())?;
        }
        if b.unknown > 0 {
            let count = meta_count(store, "unknown_count")? + b.unknown as u64;
            store.set_meta("unknown_count", Some(&count.to_string()))?;
            if !b.unknown_version.is_empty() {
                store.set_meta("unknown_version", Some(&safe_version(&b.unknown_version)))?;
            } else if store.get_meta("unknown_version")?.is_none() {
                store.set_meta("unknown_version", Some(""))?;
            }
        }
        if b.unrecognized > 0 {
            let count = meta_count(store, "unrecognized_count")? + b.unrecognized as u64;
            store.set_meta("unrecognized_count", Some(&count.to_string()))?;
            let stored = store.get_meta("unrecognized_types")?;
            let merged = merge_types(stored.as_deref(), &b.unrecognized_types);
            // A list of plain names always serialises.
            let types = serde_json::to_string(&merged).unwrap_or_else(|_| "[]".to_string());
            if stored.as_deref() != Some(types.as_str()) {
                store.set_meta("unrecognized_types", Some(&types))?;
            }
        }
        store.save_tail(next)
    })?;
    if let Some(row) = &changed {
        (ctx.row_changed)(row);
    }
    Ok(())
}

fn store_err(e: StoreError) -> io::Error {
    io::Error::other(e)
}

/// Reads new complete lines of one transcript (at most about [`MAX_BYTES_PER_PASS`] per call) and
/// stores them. Records are handed to `on_records` after each committed batch; a panic there is
/// caught and reported through `on_records_error` (TS catches what the hook throws), so the pass
/// goes on. A read or store error ends the pass and is returned.
pub fn ingest_file(
    ctx: &mut IngestCtx<'_>,
    session_id: &str,
    path: &Path,
    on_records_error: &dyn Fn(&dyn Error),
) -> io::Result<IngestResult> {
    let mut res = IngestResult::default();
    let key = path.to_string_lossy();
    let mut state = match ctx.store.get_tail(&key).map_err(store_err)? {
        Some(t) => t,
        None => initial_state(path, FIRST_INDEX_BYTES)?,
    };
    let mut consumed: u64 = 0;
    loop {
        // At most the budget left, but never below the floor (read_new may finish one longer line).
        let want = READ_CHUNK_BYTES.min(MAX_BYTES_PER_PASS.saturating_sub(consumed));
        let r = read_new(&state, MIN_READ_BYTES.max(want))?;
        if r.reset {
            // Truncated or replaced: read again from 0. A read from offset 0 never resets.
            state = r.next;
            continue;
        }
        let same = r.next.offset == state.offset
            && r.next.size == state.size
            && r.next.first_uuid == state.first_uuid;
        // Nothing new (e.g. only a partial last line): no write transaction for it.
        if r.lines.is_empty() && same && ctx.store.get_tail(&key).map_err(store_err)?.is_some() {
            return Ok(res);
        }
        let mut b = collect(session_id, &r.lines);
        commit(ctx, session_id, &mut b, &r.next).map_err(store_err)?;
        consumed += r.next.offset.saturating_sub(state.offset);
        state = r.next;
        res.lines += r.lines.len();
        res.messages += b.messages.len();
        res.unknown += b.unknown;
        res.unrecognized += b.unrecognized;
        // On a first index this replays historical records; delivery checks ignore records older
        // than the send's created_at, so the replay is harmless.
        if !b.records.is_empty()
            && let Some(hook) = ctx.on_records.as_mut()
        {
            let store = ctx.store;
            let records = &b.records;
            if catch_unwind(AssertUnwindSafe(|| hook(store, session_id, records))).is_err() {
                on_records_error(&OnRecordsPanicked);
            }
        }
        if r.lines.is_empty() {
            return Ok(res);
        }
        if consumed >= MAX_BYTES_PER_PASS {
            res.more = true;
            return Ok(res);
        }
    }
}
