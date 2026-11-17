//! Port of `test/store/db.test.ts`, plus schema parity with the TS store, the newer-schema guard, a
//! writer holding the lock, and byte compatibility of the JSON columns with `JSON.stringify`.
mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use hub_core::config::RETENTION_MS;
use hub_core::ingest::tail::TailState;
use hub_core::status::{Status, WaitingItem};
use hub_core::store::{
    ANSWERS_SQL, LatestReply, LiveStatus, MIGRATIONS, MessageRow, Role, SendRow, SendState,
    SessionRow, Store, StoreError, StoredStatus, ViewWaitingItem, iso_utc_ms,
};
use rusqlite::{Connection, OpenFlags};
use tempfile::TempDir;

const RET: i64 = RETENTION_MS as i64;

struct Db {
    _dir: TempDir,
    path: PathBuf,
}

impl Db {
    fn new() -> Db {
        let dir = tempfile::Builder::new()
            .prefix("hub-store-")
            .tempdir()
            .unwrap();
        let path = dir.path().join("index.db");
        Db { _dir: dir, path }
    }
    fn open(&self) -> Store {
        Store::open(&self.path, true).unwrap()
    }
    fn raw(&self) -> Connection {
        Connection::open(&self.path).unwrap()
    }
    fn raw_ro(&self) -> Connection {
        Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
    }
    fn file_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for p in [self.path.clone(), wal(&self.path)] {
            if let Ok(b) = std::fs::read(&p) {
                out.extend(b);
            }
        }
        out
    }
}

fn wal(p: &Path) -> PathBuf {
    PathBuf::from(format!("{}-wal", p.display()))
}
fn shm(p: &Path) -> PathBuf {
    PathBuf::from(format!("{}-shm", p.display()))
}

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

fn session() -> SessionRow {
    SessionRow {
        session_id: "s1".into(),
        profile: "work".into(),
        name: "alpha".into(),
        cwd: "C:/x".into(),
        pid: 100,
        kind: "interactive".into(),
        status: LiveStatus::Idle,
        started_at: 1000,
        updated_at: 2000,
        ended_at: None,
        transcript_path: "C:/x/s1.jsonl".into(),
        permission_mode: None,
    }
}

fn sess(id: &str, transcript: &str) -> SessionRow {
    SessionRow {
        session_id: id.into(),
        transcript_path: transcript.into(),
        ..session()
    }
}

fn msg(uuid: &str, ts: &str, text: &str, session_id: &str) -> MessageRow {
    MessageRow {
        uuid: uuid.into(),
        session_id: session_id.into(),
        ts: ts.into(),
        role: Role::Assistant,
        from_name: None,
        text: text.into(),
    }
}

fn uuids(rows: &[MessageRow]) -> Vec<&str> {
    rows.iter().map(|m| m.uuid.as_str()).collect()
}

fn ids(store: &Store) -> Vec<String> {
    store
        .list_sessions()
        .unwrap()
        .into_iter()
        .map(|v| v.session_id)
        .collect()
}

fn empty_status() -> Status {
    Status {
        progress_pct: None,
        phase: None,
        remaining: vec![],
        waiting: vec![],
        recommendations: vec![],
    }
}

fn send_base() -> SendRow {
    SendRow {
        id: None,
        session_id: "s1".into(),
        profile: "work".into(),
        name: "alpha".into(),
        sha256: "h".into(),
        created_at: 1,
        state: SendState::Pending,
        detail: None,
    }
}

fn iso(ms: i64) -> String {
    iso_utc_ms(ms)
}

// ---- sessions ----

#[test]
fn sessions_upsert_round_trips_and_updates_in_place() {
    let db = Db::new();
    let s = db.open();
    s.upsert_session(&SessionRow {
        permission_mode: Some("plan".into()),
        ..session()
    })
    .unwrap();
    s.upsert_session(&SessionRow {
        status: LiveStatus::Busy,
        updated_at: 3000,
        ..session()
    })
    .unwrap();
    let rows = s.list_sessions().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].session_id, "s1");
    assert_eq!(rows[0].state, LiveStatus::Busy);
    assert_eq!(rows[0].updated_at, 3000);
    assert_eq!(rows[0].ended_at, None);
    assert_eq!(rows[0].permission_mode, None);
}

#[test]
fn sessions_mark_ended_and_list_order() {
    let db = Db::new();
    let s = db.open();
    for (id, profile, name) in [("a", "z", "n1"), ("b", "a", "n2"), ("c", "a", "n1")] {
        s.upsert_session(&SessionRow {
            session_id: id.into(),
            profile: profile.into(),
            name: name.into(),
            ..session()
        })
        .unwrap();
    }
    s.mark_ended("c", 5000).unwrap();
    assert_eq!(ids(&s), ["b", "a", "c"]);
    assert_eq!(s.list_sessions().unwrap()[2].ended_at, Some(5000));
}

#[test]
fn sessions_get_session_reads_one_row() {
    let db = Db::new();
    let s = db.open();
    assert_eq!(s.get_session("s1").unwrap(), None);
    let row = SessionRow {
        permission_mode: Some("acceptEdits".into()),
        ended_at: Some(7),
        ..session()
    };
    s.upsert_session(&row).unwrap();
    assert_eq!(s.get_session("s1").unwrap(), Some(row));
}

// ---- tails ----

#[test]
fn tails_save_and_get() {
    let db = Db::new();
    let s = db.open();
    assert_eq!(s.get_tail("p").unwrap(), None);
    s.save_tail(&TailState {
        path: "p".into(),
        offset: 5,
        size: 9,
        first_uuid: None,
    })
    .unwrap();
    let t = TailState {
        path: "p".into(),
        offset: 7,
        size: 9,
        first_uuid: Some("u".into()),
    };
    s.save_tail(&t).unwrap();
    assert_eq!(s.get_tail("p").unwrap(), Some(t));
}

// ---- messages ----

#[test]
fn messages_dedupe_on_uuid_history_order_and_paging() {
    let db = Db::new();
    let s = db.open();
    s.upsert_session(&session()).unwrap();
    s.add_messages(
        "s1",
        &[
            msg("u1", "2026-01-01T00:00:01Z", "one", "s1"),
            msg("u2", "2026-01-01T00:00:02Z", "two", "s1"),
            msg("u3", "2026-01-01T00:00:03Z", "three", "s1"),
        ],
    )
    .unwrap();
    s.add_messages(
        "s1",
        &[
            msg("u2", "2026-01-01T00:00:02Z", "dup", "s1"),
            msg("u4", "2026-01-01T00:00:04Z", "four", "s1"),
        ],
    )
    .unwrap();
    let all = s.history("s1", 10, None).unwrap();
    assert_eq!(uuids(&all), ["u1", "u2", "u3", "u4"]);
    assert_eq!(all.iter().find(|m| m.uuid == "u2").unwrap().text, "two");
    assert_eq!(uuids(&s.history("s1", 2, None).unwrap()), ["u3", "u4"]);
    // TS `history("s1", 2, ts)` (ts-only cursor): an unknown uuid acts exactly like it.
    assert_eq!(
        uuids(
            &s.history("s1", 2, Some(("2026-01-01T00:00:03Z", "")))
                .unwrap()
        ),
        ["u1", "u2"]
    );
}

#[test]
fn messages_ts_uuid_cursor_never_skips_or_repeats_rows_sharing_a_ts() {
    let db = Db::new();
    let s = db.open();
    s.upsert_session(&session()).unwrap();
    s.add_messages(
        "s1",
        &[
            msg("t1", "2026-01-01T00:00:02Z", "one", "s1"),
            msg("t2", "2026-01-01T00:00:02Z", "two", "s1"),
            msg("t3", "2026-01-01T00:00:03Z", "three", "s1"),
        ],
    )
    .unwrap();
    let first = s.history("s1", 2, None).unwrap();
    assert_eq!(uuids(&first), ["t2", "t3"]);
    let oldest = &first[0];
    // The ts-only cursor (unknown uuid) skips the row that shares the boundary ts.
    assert!(
        s.history("s1", 2, Some((oldest.ts.as_str(), "")))
            .unwrap()
            .is_empty()
    );
    let second = s
        .history("s1", 2, Some((oldest.ts.as_str(), oldest.uuid.as_str())))
        .unwrap();
    assert_eq!(uuids(&second), ["t1"]);
    let all: Vec<&str> = uuids(&second).into_iter().chain(uuids(&first)).collect();
    assert_eq!(all, ["t1", "t2", "t3"]);
    assert_eq!(all.iter().collect::<BTreeSet<_>>().len(), 3);
}

#[test]
fn messages_cursor_of_three_rows_on_one_ts_pages_through_all() {
    let db = Db::new();
    let s = db.open();
    s.upsert_session(&session()).unwrap();
    s.add_messages(
        "s1",
        &[
            msg("a", "2026-01-01T00:00:05Z", "a", "s1"),
            msg("b", "2026-01-01T00:00:05Z", "b", "s1"),
            msg("c", "2026-01-01T00:00:05Z", "c", "s1"),
        ],
    )
    .unwrap();
    let p1 = s.history("s1", 2, None).unwrap();
    assert_eq!(uuids(&p1), ["b", "c"]);
    let p2 = s
        .history("s1", 2, Some((p1[0].ts.as_str(), p1[0].uuid.as_str())))
        .unwrap();
    assert_eq!(uuids(&p2), ["a"]);
    // An unknown uuid falls back to the ts-only cursor.
    assert!(
        s.history("s1", 2, Some((p1[0].ts.as_str(), "nope")))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn messages_history_keeps_role_and_from_name() {
    let db = Db::new();
    let s = db.open();
    s.upsert_session(&session()).unwrap();
    s.add_messages(
        "s1",
        &[MessageRow {
            uuid: "p".into(),
            session_id: "s1".into(),
            ts: "t".into(),
            role: Role::Peer,
            from_name: Some("beta".into()),
            text: "hi".into(),
        }],
    )
    .unwrap();
    let h = s.history("s1", 5, None).unwrap();
    assert_eq!(h[0].role, Role::Peer);
    assert_eq!(h[0].from_name.as_deref(), Some("beta"));
}

// ---- search ----

#[test]
fn search_finds_word_snippet_markers_newest_first_implicit_and() {
    let db = Db::new();
    let s = db.open();
    s.upsert_session(&session()).unwrap();
    s.add_messages(
        "s1",
        &[
            msg(
                "u1",
                "2026-01-01T00:00:01Z",
                "deploy the migration tonight",
                "s1",
            ),
            msg("u2", "2026-01-01T00:00:02Z", "unrelated", "s1"),
            msg("u3", "2026-01-01T00:00:03Z", "migration finished", "s1"),
        ],
    )
    .unwrap();
    let hits = s.search("migration", 10).unwrap();
    let got: Vec<&str> = hits.iter().map(|h| h.uuid.as_str()).collect();
    assert_eq!(got, ["u3", "u1"]);
    assert!(hits[0].snippet.contains("[migration]"));
    assert_eq!(hits[0].session_id, "s1");
    assert_eq!(hits[0].role, Role::Assistant);
    assert_eq!(hits[0].ts, "2026-01-01T00:00:03Z");
    let both = s.search("migration tonight", 10).unwrap();
    assert_eq!(both.len(), 1);
    assert_eq!(both[0].uuid, "u1");
}

#[test]
fn search_special_characters_do_not_break_the_query_empty_gives_nothing() {
    let db = Db::new();
    let s = db.open();
    s.upsert_session(&session()).unwrap();
    s.add_messages("s1", &[msg("u1", "t", "say \"hello\" now", "s1")])
        .unwrap();
    assert!(s.search("", 5).unwrap().is_empty());
    assert!(s.search("   ", 5).unwrap().is_empty());
    // JS `\s` includes U+3000 and U+FEFF: a query of only those is empty too.
    assert!(s.search("\u{3000}\u{feff}", 5).unwrap().is_empty());
    assert!(s.search("AND OR ( * \"", 5).unwrap().is_empty());
    let hits = s.search("\"hello\"", 5).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].uuid, "u1");
}

// ---- status ----

#[test]
fn status_set_status_set_latest_reply_session_view() {
    let db = Db::new();
    let s = db.open();
    s.upsert_session(&session()).unwrap();
    let v = &s.list_sessions().unwrap()[0];
    assert_eq!(v.state, LiveStatus::Idle);
    assert_eq!(v.status, None);
    assert_eq!(v.latest_reply, None);
    assert_eq!(v.waiting_count, 0);
    s.set_latest_reply("s1", "r1", "ts1", false).unwrap();
    assert_eq!(
        s.list_sessions().unwrap()[0].latest_reply,
        Some(LatestReply {
            reply_uuid: "r1".into(),
            reply_ts: "ts1".into(),
            has_status: false,
        })
    );
    let item = WaitingItem {
        id: Some("Q1".into()),
        text: "ok?".into(),
        is_new: true,
        children: vec!["x".into()],
    };
    s.set_status(
        "s1",
        &Status {
            progress_pct: Some(40),
            phase: Some("build".into()),
            remaining: vec!["a".into(), "b".into()],
            waiting: vec![item.clone()],
            recommendations: vec!["do it".into()],
        },
        "r2",
        "ts2",
    )
    .unwrap();
    s.set_latest_reply("s1", "r2", "ts2", true).unwrap();
    let v = &s.list_sessions().unwrap()[0];
    assert_eq!(v.waiting_count, 1);
    assert_eq!(
        v.latest_reply,
        Some(LatestReply {
            reply_uuid: "r2".into(),
            reply_ts: "ts2".into(),
            has_status: true,
        })
    );
    assert_eq!(
        v.status,
        Some(StoredStatus {
            progress_pct: Some(40),
            phase: Some("build".into()),
            remaining: vec!["a".into(), "b".into()],
            waiting: vec![ViewWaitingItem {
                item,
                answered: false
            }],
            recommendations: vec!["do it".into()],
            reply_uuid: "r2".into(),
            reply_ts: "ts2".into(),
        })
    );
}

#[test]
fn status_items_named_by_owner_or_relay_after_the_reply_are_answered() {
    let db = Db::new();
    let s = db.open();
    s.upsert_session(&session()).unwrap();
    s.upsert_session(&SessionRow {
        session_id: "s2".into(),
        name: "beta".into(),
        transcript_path: "C:/x/s2.jsonl".into(),
        ..session()
    })
    .unwrap();
    let st = |ids: &[Option<&str>]| Status {
        waiting: ids
            .iter()
            .enumerate()
            .map(|(i, id)| WaitingItem {
                id: id.map(String::from),
                text: format!("q{i}"),
                is_new: false,
                children: vec![],
            })
            .collect(),
        ..empty_status()
    };
    s.set_status(
        "s1",
        &st(&[Some("Q1"), Some("Q2"), Some("Q3"), None]),
        "r1",
        "2026-11-14T10:00:00.000Z",
    )
    .unwrap();
    s.set_status("s2", &st(&[Some("Q1")]), "r9", "2026-11-14T10:00:00.000Z")
        .unwrap();
    let row = |uuid: &str, ts: &str, role: Role, text: &str, from: Option<&str>| MessageRow {
        uuid: uuid.into(),
        session_id: "s1".into(),
        ts: ts.into(),
        role,
        from_name: from.map(String::from),
        text: text.into(),
    };
    s.add_messages(
        "s1",
        &[
            // before the status reply: does not count
            row(
                "m0",
                "2026-11-14T09:59:00.000Z",
                Role::Owner,
                "Q3 yes",
                None,
            ),
            // the owner's terminal answer and a relay answer after it
            row(
                "m1",
                "2026-11-14T10:01:00.000Z",
                Role::Owner,
                "Q1 Ignore that",
                None,
            ),
            row(
                "m2",
                "2026-11-14T10:02:00.000Z",
                Role::Peer,
                "Answers:\nQ2: go",
                Some("session-hub-relay"),
            ),
            // another peer naming Q3 and the assistant naming Q3 do not answer it
            row(
                "m3",
                "2026-11-14T10:03:00.000Z",
                Role::Peer,
                "Q3: done",
                Some("some-session"),
            ),
            row(
                "m4",
                "2026-11-14T10:04:00.000Z",
                Role::Assistant,
                "Q3 still open",
                None,
            ),
        ],
    )
    .unwrap();
    let find = |id: &str| {
        s.list_sessions()
            .unwrap()
            .into_iter()
            .find(|x| x.session_id == id)
            .unwrap()
    };
    let v = find("s1");
    let marks: Vec<(Option<String>, bool)> = v
        .status
        .as_ref()
        .unwrap()
        .waiting
        .iter()
        .map(|w| (w.item.id.clone(), w.answered))
        .collect();
    assert_eq!(
        marks,
        [
            (Some("Q1".into()), true),
            (Some("Q2".into()), true),
            (Some("Q3".into()), false),
            (None, false),
        ]
    );
    assert_eq!(v.waiting_count, 2);
    // another session's messages never answer this one's items
    assert!(!find("s2").status.unwrap().waiting[0].answered);
    // an ended session is never counted, so its items are not marked
    s.upsert_session(&SessionRow {
        ended_at: Some(9000),
        ..session()
    })
    .unwrap();
    assert!(!find("s1").status.unwrap().waiting[0].answered);
    s.upsert_session(&session()).unwrap();
    // a new status reply starts over
    s.set_status("s1", &st(&[Some("Q1")]), "r3", "2026-11-14T10:05:00.000Z")
        .unwrap();
    let w = find("s1");
    assert!(!w.status.unwrap().waiting[0].answered);
    assert_eq!(w.waiting_count, 1);
}

#[test]
fn status_json_columns_are_byte_identical_to_json_stringify() {
    let db = Db::new();
    let s = db.open();
    s.upsert_session(&session()).unwrap();
    let tricky =
        "a\"b\\c\n\t\r\u{8}\u{c}\u{1}\u{1f}\u{7f} é 😀 \u{2028} </script> \u{0}".to_string();
    s.set_status(
        "s1",
        &Status {
            progress_pct: Some(40),
            phase: Some("p".into()),
            remaining: vec!["a b".into(), "ü".into()],
            waiting: vec![
                WaitingItem {
                    id: Some("Q1".into()),
                    text: "ok?".into(),
                    is_new: true,
                    children: vec!["x".into()],
                },
                WaitingItem {
                    id: None,
                    text: tricky,
                    is_new: false,
                    children: vec![],
                },
            ],
            recommendations: vec![],
        },
        "r",
        "t",
    )
    .unwrap();
    let raw = db.raw_ro();
    let (rem, wait, rec, pct): (String, String, String, f64) = raw
        .query_row(
            "SELECT remaining_json, waiting_json, recommendations_json, progress_pct FROM status",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    // Expected strings are the output of `JSON.stringify` in Bun for the same values.
    assert_eq!(rem, "[\"a b\",\"ü\"]");
    assert_eq!(rec, "[]");
    let expected = concat!(
        r#"[{"id":"Q1","text":"ok?","isNew":true,"children":["x"]},"#,
        r#"{"id":null,"text":"a\"b\\c\n\t\r\b\f\u0001\u001f"#,
        "\u{7f} é 😀 \u{2028} </script> ",
        r#"\u0000","isNew":false,"children":[]}]"#,
    );
    assert_eq!(wait, expected);
    assert_eq!(pct, 40.0);
}

#[test]
fn status_rows_written_by_ts_are_read() {
    // A row as the TS Store writes it: JSON.stringify output and a REAL progress value.
    let db = Db::new();
    db.open().upsert_session(&session()).unwrap();
    db.raw()
        .execute_batch(
            r#"INSERT INTO status VALUES ('s1', 40, 'build', '["a"]',
               '[{"id":"Q1","text":"ok?","isNew":true,"children":["x"]},{"id":null,"text":"n","isNew":false,"children":[]}]',
               '["do it"]', 'r2', 'ts2');
               INSERT INTO latest_reply VALUES ('s1', 'r2', 'ts2', 1);"#,
        )
        .unwrap();
    let v = db.open().list_sessions().unwrap().remove(0);
    let st = v.status.unwrap();
    assert_eq!(st.progress_pct, Some(40));
    assert_eq!(st.waiting.len(), 2);
    assert_eq!(st.waiting[1].item.id, None);
    // An item without an ID is never answered, so it counts (TS: `filter((w) => !w.answered)`).
    assert_eq!(v.waiting_count, 2);
    assert!(v.latest_reply.unwrap().has_status);
}

#[test]
fn view_json_uses_ts_field_names_and_order() {
    let db = Db::new();
    let s = db.open();
    s.upsert_session(&session()).unwrap();
    s.set_status(
        "s1",
        &Status {
            waiting: vec![WaitingItem {
                id: Some("Q1".into()),
                text: "ok?".into(),
                is_new: true,
                children: vec![],
            }],
            ..empty_status()
        },
        "r",
        "t",
    )
    .unwrap();
    let v = serde_json::to_string(&s.list_sessions().unwrap()[0]).unwrap();
    assert_eq!(
        v,
        concat!(
            r#"{"sessionId":"s1","profile":"work","name":"alpha","cwd":"C:/x","pid":100,"#,
            r#""kind":"interactive","startedAt":1000,"updatedAt":2000,"endedAt":null,"#,
            r#""transcriptPath":"C:/x/s1.jsonl","permissionMode":null,"state":"idle","#,
            r#""status":{"progressPct":null,"phase":null,"remaining":[],"#,
            r#""waiting":[{"id":"Q1","text":"ok?","isNew":true,"children":[],"answered":false}],"#,
            r#""recommendations":[],"replyUuid":"r","replyTs":"t"},"latestReply":null,"#,
            r#""waitingCount":1}"#
        )
    );
    let send = serde_json::to_string(&send_base()).unwrap();
    assert_eq!(
        send,
        r#"{"sessionId":"s1","profile":"work","name":"alpha","sha256":"h","createdAt":1,"state":"pending","detail":null}"#
    );
}

// ---- answered-items query ----

#[test]
fn answered_query_starts_from_status_rows_and_reads_messages_by_index() {
    let db = Db::new();
    db.open().close().unwrap();
    let raw = db.raw();
    let mut stmt = raw
        .prepare(&format!("EXPLAIN QUERY PLAN {ANSWERS_SQL}"))
        .unwrap();
    let plan: Vec<String> = stmt
        .query_map(["x"], |r| r.get::<_, String>("detail"))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let starts = |d: &str, p: &str| {
        d.starts_with(p)
            && !d[p.len()..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
    };
    // Never a scan of messages: the TUI and the tray run this on every read of the session list.
    assert!(starts(&plan[0], "SCAN st"), "{plan:?}");
    assert!(!plan.iter().any(|d| starts(d, "SCAN m")), "{plan:?}");
    assert!(
        plan.iter()
            .any(|d| d
                .starts_with("SEARCH m USING INDEX messages_session_ts (session_id=? AND ts>?)")),
        "{plan:?}"
    );
}

// ---- meta ----

#[test]
fn meta_set_get_overwrite_none_deletes() {
    let db = Db::new();
    let s = db.open();
    assert_eq!(s.get_meta("k").unwrap(), None);
    s.set_meta("k", Some("v")).unwrap();
    assert_eq!(s.get_meta("k").unwrap().as_deref(), Some("v"));
    s.set_meta("k", Some("w")).unwrap();
    assert_eq!(s.get_meta("k").unwrap().as_deref(), Some("w"));
    s.set_meta("k", None).unwrap();
    assert_eq!(s.get_meta("k").unwrap(), None);
}

// ---- sends ----

#[test]
fn sends_create_open_send_for_update() {
    let db = Db::new();
    let s = db.open();
    let a = s.create_send(&send_base()).unwrap();
    let b = s
        .create_send(&SendRow {
            created_at: 2,
            state: SendState::Held,
            ..send_base()
        })
        .unwrap();
    assert!(b > a);
    assert_eq!(s.open_send_for("s1").unwrap().unwrap().id, Some(b));
    assert!(
        s.transition_send(b, SendState::Delivered, Some(Some("ok")))
            .unwrap()
    );
    assert_eq!(
        s.get_send(b).unwrap().unwrap().detail.as_deref(),
        Some("ok")
    );
    assert_eq!(s.open_send_for("s1").unwrap().unwrap().id, Some(a));
    assert!(s.transition_send(a, SendState::Expired, None).unwrap());
    assert_eq!(s.open_send_for("s1").unwrap(), None);
    assert_eq!(s.open_send_for("nope").unwrap(), None);
}

#[test]
fn sends_get_send_reads_one_send_in_any_state() {
    let db = Db::new();
    let s = db.open();
    let id = s
        .create_send(&SendRow {
            created_at: 5,
            ..send_base()
        })
        .unwrap();
    assert_eq!(s.get_send(id).unwrap().unwrap().state, SendState::Pending);
    s.transition_send(id, SendState::Delivered, Some(Some("ok")))
        .unwrap();
    assert_eq!(
        s.get_send(id).unwrap(),
        Some(SendRow {
            id: Some(id),
            created_at: 5,
            state: SendState::Delivered,
            detail: Some("ok".into()),
            ..send_base()
        })
    );
    assert_eq!(s.get_send(id + 1).unwrap(), None);
}

#[test]
fn sends_transition_rules() {
    // db.ts transitionSend: from pending/held to anything, from failed only to delivered (clearing
    // the detail when `detail` is not given); delivered and expired never change.
    let db = Db::new();
    let s = db.open();
    let f = s.create_send(&send_base()).unwrap();
    assert!(
        s.transition_send(f, SendState::Failed, Some(Some("relay said no")))
            .unwrap()
    );
    assert!(!s.transition_send(f, SendState::Expired, None).unwrap());
    assert!(!s.transition_send(f, SendState::Pending, None).unwrap());
    assert!(s.transition_send(f, SendState::Delivered, None).unwrap());
    assert_eq!(s.get_send(f).unwrap().unwrap().detail, None);
    assert!(!s.transition_send(f, SendState::Failed, None).unwrap());
    let g = s.create_send(&send_base()).unwrap();
    s.transition_send(g, SendState::Failed, Some(Some("x")))
        .unwrap();
    assert!(
        s.transition_send(g, SendState::Delivered, Some(Some("late")))
            .unwrap()
    );
    assert_eq!(
        s.get_send(g).unwrap().unwrap().detail.as_deref(),
        Some("late")
    );
    let h = s
        .create_send(&SendRow {
            detail: Some("kept".into()),
            ..send_base()
        })
        .unwrap();
    assert!(s.transition_send(h, SendState::Held, None).unwrap());
    assert_eq!(
        s.get_send(h).unwrap().unwrap().detail.as_deref(),
        Some("kept")
    );
    assert!(
        s.transition_send(h, SendState::Expired, Some(None))
            .unwrap()
    );
    assert_eq!(s.get_send(h).unwrap().unwrap().detail, None);
    assert!(!s.transition_send(h, SendState::Delivered, None).unwrap());
    assert!(!s.transition_send(999, SendState::Delivered, None).unwrap());
}

#[test]
fn sends_queries_by_time_and_state() {
    let db = Db::new();
    let s = db.open();
    let mk = |sid: &str, at: i64, sha: &str| SendRow {
        session_id: sid.into(),
        created_at: at,
        sha256: sha.into(),
        ..send_base()
    };
    let a = s.create_send(&mk("s1", 10, "ha")).unwrap();
    let b = s.create_send(&mk("s2", 20, "hb")).unwrap();
    let c = s.create_send(&mk("s1", 30, "hc")).unwrap();
    let d = s.create_send(&mk("s1", 40, "hd")).unwrap();
    s.transition_send(c, SendState::Failed, None).unwrap();
    s.transition_send(d, SendState::Delivered, None).unwrap();
    assert_eq!(s.count_sends_since(20).unwrap(), 3);
    assert_eq!(s.count_sends_since(41).unwrap(), 0);
    let all: Vec<_> = s.open_sends(None).unwrap().iter().map(|r| r.id).collect();
    assert_eq!(all, [Some(a), Some(b)]);
    let one: Vec<_> = s
        .open_sends(Some("s1"))
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(one, [Some(a)]);
    assert_eq!(
        s.send_hashes_since("s1", 30).unwrap(),
        BTreeSet::from(["hc".to_string(), "hd".to_string()])
    );
    let verify: Vec<_> = s
        .sends_to_verify("s1", 30)
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(verify, [Some(a), Some(c)]);
    let verify_late: Vec<_> = s
        .sends_to_verify("s1", 31)
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(verify_late, [Some(a)]);
}

// ---- retention leaves no residue (PII-03) ----

#[test]
fn retention_old_messages_go_from_live_sessions_too() {
    let db = Db::new();
    let s = db.open();
    let now = 10 * RET;
    s.upsert_session(&sess("live", "live.jsonl")).unwrap();
    s.add_messages(
        "live",
        &[
            msg("old-1", &iso(now - RET - 60_000), "walrusold", "live"),
            msg("new-1", &iso(now - 60_000), "walrusnew", "live"),
        ],
    )
    .unwrap();
    s.purge(now).unwrap();
    assert_eq!(ids(&s), ["live"]);
    assert_eq!(uuids(&s.history("live", 10, None).unwrap()), ["new-1"]);
    assert!(s.search("walrusold", 10).unwrap().is_empty());
    assert_eq!(s.search("walrusnew", 10).unwrap().len(), 1);
}

#[test]
fn retention_live_status_and_latest_reply_older_than_window_go() {
    let db = Db::new();
    let s = db.open();
    let now = 10 * RET;
    s.upsert_session(&sess("live", "live.jsonl")).unwrap();
    s.upsert_session(&sess("fresh", "fresh.jsonl")).unwrap();
    let st = Status {
        progress_pct: Some(5),
        phase: Some("old phase text".into()),
        remaining: vec!["r".into()],
        ..empty_status()
    };
    s.set_status("live", &st, "r-old", &iso(now - RET - 1000))
        .unwrap();
    s.set_latest_reply("live", "r-old", &iso(now - RET - 1000), true)
        .unwrap();
    s.set_status("fresh", &st, "r-new", &iso(now - 1000))
        .unwrap();
    s.set_latest_reply("fresh", "r-new", &iso(now - 1000), true)
        .unwrap();
    s.purge(now).unwrap();
    let by_id: BTreeMap<String, _> = s
        .list_sessions()
        .unwrap()
        .into_iter()
        .map(|v| (v.session_id.clone(), v))
        .collect();
    assert_eq!(by_id["live"].status, None);
    assert_eq!(by_id["live"].latest_reply, None);
    assert_eq!(
        by_id["fresh"].status.as_ref().unwrap().phase.as_deref(),
        Some("old phase text")
    );
}

#[test]
fn retention_large_backlog_is_purged_in_batches() {
    let db = Db::new();
    let s = db.open();
    let now = 10 * RET;
    s.upsert_session(&sess("live", "live.jsonl")).unwrap();
    let rows: Vec<MessageRow> = (0..1234)
        .map(|i| {
            msg(
                &format!("b-{i}"),
                &iso(now - RET - 10_000 + i),
                &format!("old {i}"),
                "live",
            )
        })
        .collect();
    s.add_messages("live", &rows).unwrap();
    s.purge(now).unwrap();
    assert!(s.history("live", 5000, None).unwrap().is_empty());
}

#[test]
fn retention_purged_text_not_left_in_files_after_vacuum() {
    let db = Db::new();
    let s = db.open();
    let now = 10 * RET;
    s.upsert_session(&sess("old", "old.jsonl")).unwrap();
    s.upsert_session(&sess("live", "live.jsonl")).unwrap();
    s.mark_ended("old", now - RET - 1).unwrap();
    let canary = "quokkacanary7731";
    s.add_messages(
        "old",
        &[msg(
            "c-1",
            &iso(now - RET - 5),
            &format!("{canary} {canary}"),
            "old",
        )],
    )
    .unwrap();
    let keep: Vec<MessageRow> = (0..50)
        .map(|i| {
            msg(
                &format!("k-{i}"),
                &iso(now - 1000),
                &format!("keep {i}"),
                "live",
            )
        })
        .collect();
    s.add_messages("live", &keep).unwrap();
    assert_eq!(s.search(canary, 10).unwrap().len(), 1);
    s.purge(now).unwrap();
    s.vacuum().unwrap();
    assert!(!contains(&db.file_bytes(), canary));
    assert_eq!(s.search("keep", 100).unwrap().len(), 50);
}

#[test]
fn purge_leaves_no_wal_residue_right_after_the_purge() {
    let db = Db::new();
    let s = db.open();
    let now = 10 * RET;
    s.upsert_session(&sess("live", "live.jsonl")).unwrap();
    let canary = "wombatcanary4417";
    s.add_messages(
        "live",
        &[msg(
            "c-1",
            &iso(now - RET - 5),
            &format!("{canary} {canary}"),
            "live",
        )],
    )
    .unwrap();
    s.add_messages("live", &[msg("k-1", &iso(now - 1000), "keep this", "live")])
        .unwrap();
    assert_eq!(s.search(canary, 10).unwrap().len(), 1);
    s.purge(now).unwrap();
    assert!(!contains(&db.file_bytes(), canary));
    assert_eq!(s.search("keep", 10).unwrap().len(), 1);
}

#[test]
fn purge_removes_old_ended_sessions_keeps_live_and_recent() {
    let db = Db::new();
    let s = db.open();
    let now = 10 * RET;
    for id in ["old", "recent", "live"] {
        s.upsert_session(&sess(id, &format!("{id}.jsonl"))).unwrap();
    }
    s.mark_ended("old", now - RET - 1).unwrap();
    s.mark_ended("recent", now - RET + 1000).unwrap();
    for id in ["old", "recent", "live"] {
        s.add_messages(id, &[msg(&format!("m-{id}"), "t", "zebra", id)])
            .unwrap();
        s.set_latest_reply(id, "r", "t", false).unwrap();
        s.set_status(id, &empty_status(), "r", "t").unwrap();
        s.save_tail(&TailState {
            path: format!("{id}.jsonl"),
            offset: 1,
            size: 1,
            first_uuid: None,
        })
        .unwrap();
    }
    let send = SendRow {
        session_id: "old".into(),
        profile: "p".into(),
        name: "n".into(),
        sha256: "h".into(),
        state: SendState::Delivered,
        detail: None,
        created_at: now - RET - 1,
        id: None,
    };
    s.create_send(&send).unwrap();
    let keep = s
        .create_send(&SendRow {
            state: SendState::Pending,
            created_at: now - 1000,
            ..send.clone()
        })
        .unwrap();

    assert_eq!(s.purge(now).unwrap(), 1);
    let mut left = ids(&s);
    left.sort();
    assert_eq!(left, ["live", "recent"]);
    assert!(s.history("old", 10, None).unwrap().is_empty());
    assert_eq!(s.get_tail("old.jsonl").unwrap(), None);
    assert!(s.get_tail("live.jsonl").unwrap().is_some());
    let mut found: Vec<String> = s
        .search("zebra", 10)
        .unwrap()
        .into_iter()
        .map(|h| h.session_id)
        .collect();
    found.sort();
    assert_eq!(found, ["live", "recent"]);
    assert_eq!(s.open_send_for("old").unwrap().unwrap().id, Some(keep));
    s.transition_send(keep, SendState::Failed, None).unwrap();
    s.vacuum().unwrap();
    assert_eq!(s.search("zebra", 10).unwrap().len(), 2);
}

// ---- migrations ----

#[test]
fn migrations_reopen_is_idempotent_and_keeps_data() {
    let db = Db::new();
    let a = db.open();
    a.upsert_session(&session()).unwrap();
    a.close().unwrap();
    let b = db.open();
    assert_eq!(b.list_sessions().unwrap().len(), 1);
    let c = db.open();
    assert_eq!(c.list_sessions().unwrap().len(), 1);
}

#[test]
fn migrations_v2_resets_inflated_unknown_count_once_keeping_other_meta() {
    let db = Db::new();
    {
        let raw = db.raw();
        raw.execute_batch("CREATE TABLE schema_version (version INTEGER NOT NULL)")
            .unwrap();
        raw.execute_batch(MIGRATIONS[0]).unwrap();
        raw.execute_batch(
            "INSERT INTO schema_version (version) VALUES (1);
             INSERT INTO meta (key, value) VALUES ('unknown_count', '1441'), ('unknown_version', '2.1.300'), ('stale_since:claude', '5');",
        )
        .unwrap();
    }
    let a = db.open();
    assert_eq!(a.get_meta("unknown_count").unwrap().as_deref(), Some("0"));
    assert_eq!(
        a.get_meta("unknown_version").unwrap().as_deref(),
        Some("2.1.300")
    );
    assert_eq!(
        a.get_meta("stale_since:claude").unwrap().as_deref(),
        Some("5")
    );
    a.set_meta("unknown_count", Some("2")).unwrap();
    a.close().unwrap();
    assert_eq!(
        db.open().get_meta("unknown_count").unwrap().as_deref(),
        Some("2")
    );
}

#[test]
fn migrations_fresh_index_is_at_latest_version_with_no_unknown_count() {
    let db = Db::new();
    let s = db.open();
    assert_eq!(s.get_meta("unknown_count").unwrap(), None);
    let v: i64 = db
        .raw_ro()
        .query_row("SELECT MAX(version) AS v FROM schema_version", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(v, MIGRATIONS.len() as i64);
    assert_eq!(MIGRATIONS.len(), 5);
}

#[test]
fn migrations_v3_turns_on_fts_secure_delete_for_a_v2_index() {
    let db = Db::new();
    {
        let raw = db.raw();
        raw.execute_batch("CREATE TABLE schema_version (version INTEGER NOT NULL)")
            .unwrap();
        raw.execute_batch(MIGRATIONS[0]).unwrap();
        raw.execute_batch(MIGRATIONS[1]).unwrap();
        raw.execute_batch(
            "INSERT INTO schema_version (version) VALUES (1), (2);
             INSERT INTO sessions VALUES ('s1','work','alpha','C:/x',1,'interactive','idle',1,2,NULL,'C:/x/s1.jsonl',NULL);
             INSERT INTO messages (uuid, session_id, ts, role, from_name, text) VALUES ('u1','s1','2026-11-13T00:00:00.000Z','assistant',NULL,'pelican');",
        )
        .unwrap();
    }
    let s = db.open();
    assert_eq!(s.search("pelican", 10).unwrap().len(), 1);
    let v: i64 = db
        .raw_ro()
        .query_row(
            "SELECT v FROM messages_fts_config WHERE k = 'secure-delete'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(v, 1);
}

// ---- v4: stored timestamps become ISO UTC (DATA-07) ----

/// A v3 index written before ingest normalised timestamps: empty, offset and unparseable values.
fn v3_index(db: &Db, now: i64) {
    let raw = db.raw();
    raw.execute_batch("CREATE TABLE schema_version (version INTEGER NOT NULL)")
        .unwrap();
    for m in &MIGRATIONS[..3] {
        raw.execute_batch(m).unwrap();
    }
    raw.execute_batch("INSERT INTO schema_version (version) VALUES (1), (2), (3)")
        .unwrap();
    raw.execute(
        "INSERT INTO sessions VALUES ('s1','work','alpha','C:/x',1,'interactive','idle',1,?1,NULL,'C:/x/s1.jsonl',NULL)",
        [now],
    )
    .unwrap();
    let mut ins = raw
        .prepare(
            "INSERT INTO messages (uuid, session_id, ts, role, from_name, text) VALUES (?1, 's1', ?2, 'assistant', NULL, ?3)",
        )
        .unwrap();
    for (u, ts, text) in [
        ("empty", "", "recent but undated"),
        (
            "offset",
            "2026-11-14T10:00:00+05:30",
            "recent with an offset",
        ),
        ("old", "2020-01-01T00:00:00+05:30", "years old"),
        ("junk", "not a date", "unparseable"),
        ("iso", "2026-11-14T04:00:00.000Z", "already ISO"),
    ] {
        ins.execute([u, ts, text]).unwrap();
    }
    raw.execute_batch(
        "INSERT INTO status VALUES ('s1', 50, 'p', '[]', '[]', '[]', 'empty', '');
         INSERT INTO latest_reply VALUES ('s1', 'offset', '2026-11-14T10:00:00+05:30', 1);",
    )
    .unwrap();
}

const V4_NOW: i64 = 1_794_636_000_123; // 2026-11-14T06:00:00.123Z

#[test]
fn v4_normalises_messages_status_latest_reply_empty_or_junk_take_updated_at() {
    assert_eq!(iso(V4_NOW), "2026-11-14T06:00:00.123Z");
    let db = Db::new();
    v3_index(&db, V4_NOW);
    db.open().close().unwrap();
    let raw = db.raw_ro();
    let mut stmt = raw.prepare("SELECT uuid, ts FROM messages").unwrap();
    let ts: BTreeMap<String, String> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let st: String = raw
        .query_row("SELECT reply_ts AS t FROM status", [], |r| r.get(0))
        .unwrap();
    let lr: String = raw
        .query_row("SELECT reply_ts AS t FROM latest_reply", [], |r| r.get(0))
        .unwrap();
    let fallback = iso(V4_NOW);
    let expected: BTreeMap<String, String> = [
        ("empty", fallback.as_str()),
        ("offset", "2026-11-14T04:30:00.000Z"),
        ("old", "2019-12-31T18:30:00.000Z"),
        ("junk", fallback.as_str()),
        ("iso", "2026-11-14T04:00:00.000Z"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    assert_eq!(ts, expected);
    assert_eq!(st, fallback);
    assert_eq!(lr, "2026-11-14T04:30:00.000Z");
}

#[test]
fn v4_first_purge_after_upgrade_keeps_recent_rows_drops_only_old() {
    let db = Db::new();
    v3_index(&db, V4_NOW);
    let s = db.open();
    s.purge(V4_NOW).unwrap();
    let mut got: Vec<String> = s
        .history("s1", 50, None)
        .unwrap()
        .into_iter()
        .map(|m| m.uuid)
        .collect();
    got.sort();
    assert_eq!(got, ["empty", "iso", "junk", "offset"]);
    let view = s.list_sessions().unwrap().remove(0);
    assert!(view.status.is_some());
    assert!(view.latest_reply.is_some());
}

#[test]
fn v4_fts_still_finds_a_message_whose_timestamp_changed() {
    let db = Db::new();
    v3_index(&db, V4_NOW);
    assert_eq!(db.open().search("offset", 10).unwrap().len(), 1);
}

#[test]
fn v4_fts_update_trigger_fires_only_on_text_changes_and_reindexes_them() {
    let db = Db::new();
    v3_index(&db, V4_NOW);
    db.open().close().unwrap();
    {
        let raw = db.raw();
        let sql: String = raw
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'messages_au'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(sql.contains("AFTER UPDATE OF text ON messages"));
        raw.execute_batch("UPDATE messages SET text = 'heron' WHERE uuid = 'offset'")
            .unwrap();
    }
    let s = db.open();
    assert_eq!(s.search("heron", 10).unwrap().len(), 1);
    assert!(s.search("offset", 10).unwrap().is_empty());
}

#[test]
fn v4_row_whose_session_is_missing_takes_the_migration_time() {
    let db = Db::new();
    {
        let raw = db.raw();
        raw.execute_batch("CREATE TABLE schema_version (version INTEGER NOT NULL)")
            .unwrap();
        for m in &MIGRATIONS[..3] {
            raw.execute_batch(m).unwrap();
        }
        raw.execute_batch(
            "INSERT INTO schema_version (version) VALUES (1), (2), (3);
             PRAGMA foreign_keys = OFF;
             INSERT INTO messages (uuid, session_id, ts, role, from_name, text) VALUES ('orphan', 'gone', '', 'assistant', NULL, 'x');",
        )
        .unwrap();
    }
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    db.open().close().unwrap();
    let ts: String = db
        .raw_ro()
        .query_row("SELECT ts FROM messages WHERE uuid = 'orphan'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let re =
        regex::Regex::new(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{3}Z$")
            .unwrap();
    assert!(re.is_match(&ts), "{ts}");
    // ISO strings of one width compare like their instants.
    assert!(ts >= iso(before - 1000), "{ts}");
}

// ---- v5: owner rows that were Claude Code's text are dropped ----

#[test]
fn v5_drops_indexed_compaction_summaries_and_local_command_output_keeping_the_owners_own_text() {
    let db = Db::new();
    let raw = db.raw();
    raw.execute_batch("CREATE TABLE schema_version (version INTEGER NOT NULL)")
        .unwrap();
    for m in &MIGRATIONS[..4] {
        raw.execute_batch(m).unwrap();
    }
    raw.execute_batch("INSERT INTO schema_version (version) VALUES (1), (2), (3), (4)")
        .unwrap();
    raw.execute_batch(
        "INSERT INTO sessions VALUES ('s1','work','alpha','C:/x',1,'interactive','idle',1,2,NULL,'C:/x/s1.jsonl',NULL)",
    )
    .unwrap();
    for (uuid, role, text) in [
        (
            "m1",
            "owner",
            "This session is being continued from a previous conversation. Q7: yes",
        ),
        (
            "m2",
            "owner",
            "<local-command-stdout>Compacted</local-command-stdout>",
        ),
        (
            "m3",
            "owner",
            "<local-command-stderr>oops</local-command-stderr>",
        ),
        ("m4", "owner", "Q7 yes go"),
        ("m5", "owner", "<command-name>/compact</command-name>"),
        (
            "m6",
            "assistant",
            "<local-command-stdout>quoted by the session</local-command-stdout>",
        ),
    ] {
        raw.execute(
            "INSERT INTO messages (uuid, session_id, ts, role, from_name, text) VALUES (?1, 's1', '2026-11-15T09:00:00.000Z', ?2, NULL, ?3)",
            [uuid, role, text],
        )
        .unwrap();
    }
    drop(raw);
    db.open().close().unwrap();
    let check = db.raw_ro();
    let mut stmt = check
        .prepare("SELECT uuid FROM messages ORDER BY uuid")
        .unwrap();
    let left: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let fts: i64 = check
        .query_row(
            "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'Compacted'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(left, ["m4", "m5", "m6"]);
    assert_eq!(fts, 0);
}

// ---- concurrency ----

const WRITER_ENV: &str = "HUB_STORE_WRITER_DB";

/// Child process for the cross-process concurrency test: acts as the collector daemon
/// (`test/store/writer.ts`). Does nothing unless started by that test.
#[test]
#[ignore = "child process of concurrency_separate_process_writes_while_this_process_reads"]
fn writer_child() {
    let Ok(path) = std::env::var(WRITER_ENV) else {
        return;
    };
    let store = Store::open(Path::new(&path), true).unwrap();
    let base = SessionRow {
        pid: 1,
        started_at: 1,
        updated_at: 1,
        ..session()
    };
    store
        .upsert_session(&SessionRow {
            session_id: "s1".into(),
            ended_at: None,
            transcript_path: "s1.jsonl".into(),
            ..base.clone()
        })
        .unwrap();
    store
        .upsert_session(&SessionRow {
            session_id: "old".into(),
            ended_at: Some(1),
            transcript_path: "old.jsonl".into(),
            ..base
        })
        .unwrap();
    store
        .add_messages(
            "old",
            &[MessageRow {
                role: Role::Owner,
                ..msg("old-1", "t", "ancient", "old")
            }],
        )
        .unwrap();
    let mut n = 0;
    for batch in 0..200 {
        let mut rows = Vec::new();
        for _ in 0..10 {
            rows.push(msg(
                &format!("u{n}"),
                &format!("2026-01-01T00:00:00.{n:04}Z"),
                &format!("word{n} common"),
                "s1",
            ));
            n += 1;
        }
        store.add_messages("s1", &rows).unwrap();
        if batch == 100 {
            store.purge(RET * 10).unwrap();
            store.vacuum().unwrap();
        }
    }
    store.close().unwrap();
}

#[test]
fn concurrency_separate_process_writes_while_this_process_reads() {
    let db = Db::new();
    let reader = db.open();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["writer_child", "--exact", "--ignored", "--test-threads=1"])
        .env(WRITER_ENV, &db.path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let mut loops = 0;
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        reader.list_sessions().unwrap();
        reader.history("s1", 20, None).unwrap();
        reader.search("common", 5).unwrap();
        loops += 1;
        assert!(started.elapsed() < Duration::from_secs(60), "writer hung");
    };
    let mut err = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut err).unwrap();
    assert!(status.success(), "writer failed: {err}");
    assert!(loops > 0);
    assert_eq!(reader.history("s1", 5000, None).unwrap().len(), 2000);
    assert_eq!(reader.search("common", 5000).unwrap().len(), 2000);
    assert!(reader.search("ancient", 5).unwrap().is_empty());
    assert_eq!(ids(&reader), ["s1"]);
}

#[test]
fn concurrency_two_handles_writing_interleaved() {
    let db = Db::new();
    let a = db.open();
    let b = db.open();
    a.upsert_session(&session()).unwrap();
    let job = |st: Store, tag: &'static str| {
        std::thread::spawn(move || {
            for i in 0..100 {
                st.add_messages("s1", &[msg(&format!("{tag}{i}"), "t", "x", "s1")])
                    .unwrap();
            }
            st
        })
    };
    let ta = job(a, "a");
    let tb = job(b, "b");
    let a = ta.join().unwrap();
    tb.join().unwrap();
    assert_eq!(a.history("s1", 1000, None).unwrap().len(), 200);
}

#[test]
fn waits_for_writer() {
    let db = Db::new();
    // Opened before the other connection takes the write lock, as the TUI and daemon are.
    let s = db.open();
    let (locked_tx, locked_rx) = mpsc::channel();
    let path = db.path.clone();
    let holder = std::thread::spawn(move || {
        let raw = Connection::open(&path).unwrap();
        raw.execute_batch("BEGIN IMMEDIATE").unwrap();
        locked_tx.send(()).unwrap();
        std::thread::sleep(Duration::from_secs(2));
        raw.execute_batch("COMMIT").unwrap();
    });
    locked_rx.recv().unwrap();
    let started = Instant::now();
    s.upsert_session(&session()).unwrap();
    let waited = started.elapsed();
    holder.join().unwrap();
    assert!(
        waited >= Duration::from_millis(1000),
        "did not wait: {waited:?}"
    );
    assert_eq!(ids(&s), ["s1"]);
}

// ---- tx ----

#[test]
fn tx_commits_together_and_rolls_back_on_error() {
    let db = Db::new();
    let s = db.open();
    s.tx(|| -> Result<(), StoreError> {
        s.upsert_session(&session())?;
        s.set_latest_reply("s1", "r", "t", false)
    })
    .unwrap();
    assert!(s.list_sessions().unwrap()[0].latest_reply.is_some());
    #[derive(Debug)]
    enum E {
        Boom,
        Store,
    }
    impl From<StoreError> for E {
        fn from(_: StoreError) -> E {
            E::Store
        }
    }
    let r = s.tx(|| -> Result<(), E> {
        s.upsert_session(&sess("s2", "x"))?;
        Err(E::Boom)
    });
    assert!(matches!(r, Err(E::Boom)));
    assert_eq!(ids(&s), ["s1"]);
    s.add_messages("s1", &[msg("after", "t", "still works", "s1")])
        .unwrap();
    assert_eq!(s.history("s1", 5, None).unwrap().len(), 1);
}

#[test]
fn tx_nests_add_messages_and_rolls_back_inner_writes() {
    let db = Db::new();
    let s = db.open();
    s.upsert_session(&session()).unwrap();
    let r = s.tx(|| -> Result<(), StoreError> {
        s.add_messages("s1", &[msg("in", "t", "inside", "s1")])?;
        // Foreign key: no such session, so this insert fails and the whole tx rolls back.
        s.add_messages(
            "no-such-session",
            &[msg("bad", "t", "x", "no-such-session")],
        )
    });
    assert!(r.is_err());
    assert!(s.history("s1", 5, None).unwrap().is_empty());
    s.tx(|| s.add_messages("s1", &[msg("in", "t", "inside", "s1")]))
        .unwrap();
    assert_eq!(s.history("s1", 5, None).unwrap().len(), 1);
}

// ---- open guards ----

#[test]
fn refuses_newer_schema() {
    let db = Db::new();
    db.open().close().unwrap();
    db.raw()
        .execute_batch("INSERT INTO schema_version (version) VALUES (6)")
        .unwrap();
    let err = Store::open(&db.path, true).err().unwrap();
    assert!(
        matches!(
            err,
            StoreError::NewerSchema {
                found: 6,
                supported: 5
            }
        ),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "hub.db has schema version 6, newer than this build supports (5)"
    );
    let err = Store::open_read_only(&db.path).err().unwrap();
    assert!(matches!(
        err,
        StoreError::NewerSchema {
            found: 6,
            supported: 5
        }
    ));
    // The refused open released the file.
    std::fs::remove_file(&db.path).unwrap();
    let _ = std::fs::remove_file(wal(&db.path));
    let _ = std::fs::remove_file(shm(&db.path));
}

#[test]
fn open_guards_newer_schema_999_throws_and_releases_the_file() {
    let db = Db::new();
    db.open().close().unwrap();
    db.raw()
        .execute_batch("INSERT INTO schema_version (version) VALUES (999)")
        .unwrap();
    let err = Store::open(&db.path, true).err().unwrap();
    assert!(err.to_string().contains("newer"));
    std::fs::remove_file(&db.path).unwrap();
    let _ = std::fs::remove_file(wal(&db.path));
    let _ = std::fs::remove_file(shm(&db.path));
}

#[test]
fn open_without_create_on_missing_path_fails_and_creates_no_file() {
    let db = Db::new();
    assert!(Store::open(&db.path, false).is_err());
    assert!(!db.path.exists());
    assert!(!wal(&db.path).exists());
    assert!(Store::open_read_only(&db.path).is_err());
    assert!(!db.path.exists());
}

#[test]
fn open_without_create_opens_an_existing_index() {
    let db = Db::new();
    let a = db.open();
    a.upsert_session(&session()).unwrap();
    let b = Store::open(&db.path, false).unwrap();
    assert_eq!(ids(&b), ["s1"]);
}

#[test]
fn open_read_only_reads_and_never_writes() {
    let db = Db::new();
    let w = db.open();
    w.upsert_session(&session()).unwrap();
    w.close().unwrap();
    // No writer open: the -wal and -shm files were removed with the last connection.
    let r = Store::open_read_only(&db.path).unwrap();
    assert_eq!(ids(&r), ["s1"]);
    assert!(r.upsert_session(&sess("s2", "y")).is_err());
    // A writer that commits later is seen by the reader.
    let w = db.open();
    let v0 = r.data_version().unwrap();
    w.upsert_session(&sess("s2", "y")).unwrap();
    assert_ne!(r.data_version().unwrap(), v0);
    assert_eq!(ids(&r), ["s1", "s2"]);
}

#[test]
fn open_read_only_requires_wal() {
    let db = Db::new();
    {
        let raw = db.raw();
        raw.execute_batch("CREATE TABLE schema_version (version INTEGER NOT NULL)")
            .unwrap();
    }
    let err = Store::open_read_only(&db.path).err().unwrap();
    assert!(matches!(err, StoreError::WalUnavailable), "{err:?}");
}

// ---- schema parity with the TS store ----

#[test]
fn migrations_are_verbatim_copies_of_schema_ts() {
    let ts = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../src/store/schema.ts"),
    )
    .unwrap();
    let literals: Vec<&str> = ts.split('`').skip(1).step_by(2).collect();
    assert_eq!(literals, MIGRATIONS);
}

#[test]
fn schema_matches_ts() {
    let db = Db::new();
    db.open().close().unwrap();
    let golden = common::golden_value("schema");
    let raw = db.raw_ro();
    let mut stmt = raw
        .prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY name")
        .unwrap();
    let master: Vec<serde_json::Value> = stmt
        .query_map([], |r| {
            Ok(serde_json::json!({
                "type": r.get::<_, String>(0)?,
                "name": r.get::<_, String>(1)?,
                "tbl_name": r.get::<_, String>(2)?,
                "sql": r.get::<_, Option<String>>(3)?,
            }))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(serde_json::Value::Array(master), golden["sqlite_master"]);
    let mut stmt = raw
        .prepare("SELECT version FROM schema_version ORDER BY version")
        .unwrap();
    let versions: Vec<serde_json::Value> = stmt
        .query_map([], |r| {
            Ok(serde_json::json!({ "version": r.get::<_, i64>(0)? }))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        serde_json::Value::Array(versions),
        golden["schema_version_rows"]
    );
    let max: i64 = raw
        .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(serde_json::json!(max), golden["schema_version_max"]);
    let uv: i64 = raw
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(serde_json::json!(uv), golden["pragma_user_version"]);
    let mode: String = raw
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
}

// ---- fts ----

fn fts_docs(db: &Db) -> i64 {
    db.raw_ro()
        .query_row("SELECT COUNT(*) AS n FROM messages_fts_docsize", [], |r| {
            r.get(0)
        })
        .unwrap()
}

#[test]
fn fts_dedupe_does_not_double_index_purge_removes_rows_vacuum_keeps_search() {
    let db = Db::new();
    let s = db.open();
    s.upsert_session(&sess("old", "o")).unwrap();
    s.upsert_session(&sess("live", "l")).unwrap();
    s.add_messages("old", &[msg("a", "t", "zebra", "old")])
        .unwrap();
    s.add_messages("live", &[msg("b", "t", "zebra", "live")])
        .unwrap();
    s.add_messages("live", &[msg("b", "t", "zebra", "live")])
        .unwrap();
    assert_eq!(fts_docs(&db), 2);
    s.mark_ended("old", 1).unwrap();
    assert_eq!(s.purge(RET * 10).unwrap(), 1);
    assert_eq!(fts_docs(&db), 1);
    s.vacuum().unwrap();
    assert_eq!(fts_docs(&db), 1);
    let hits: Vec<String> = s
        .search("zebra", 5)
        .unwrap()
        .into_iter()
        .map(|h| h.uuid)
        .collect();
    assert_eq!(hits, ["b"]);
}

#[test]
fn purge_keeps_a_tail_shared_with_a_live_session() {
    let db = Db::new();
    let s = db.open();
    let now = 10 * RET;
    s.upsert_session(&sess("old", "shared")).unwrap();
    s.upsert_session(&sess("live", "shared")).unwrap();
    s.upsert_session(&sess("gone", "own")).unwrap();
    s.mark_ended("old", 1).unwrap();
    s.mark_ended("gone", 1).unwrap();
    for p in ["shared", "own"] {
        s.save_tail(&TailState {
            path: p.into(),
            offset: 1,
            size: 1,
            first_uuid: None,
        })
        .unwrap();
    }
    assert_eq!(s.purge(now).unwrap(), 2);
    assert!(s.get_tail("shared").unwrap().is_some());
    assert_eq!(s.get_tail("own").unwrap(), None);
}

// ---- data_version ----

#[test]
fn data_version_changes_after_another_connection_commits_not_after_own_write() {
    let db = Db::new();
    let a = db.open();
    let b = db.open();
    let v0 = a.data_version().unwrap();
    b.set_meta("x", Some("1")).unwrap();
    let v1 = a.data_version().unwrap();
    assert_ne!(v1, v0);
    a.set_meta("y", Some("2")).unwrap();
    assert_eq!(a.data_version().unwrap(), v1);
}

// ---- helpers ----

#[test]
fn iso_utc_ms_matches_js_to_iso_string() {
    // Expected values: `new Date(ms).toISOString()` in Bun.
    for (ms, want) in [
        (0, "1970-01-01T00:00:00.000Z"),
        (-1, "1969-12-31T23:59:59.999Z"),
        (951_782_400_000, "2000-02-29T00:00:00.000Z"),
        (253_402_300_800_000, "+010000-01-01T00:00:00.000Z"),
        (-62_198_755_200_000, "-000001-01-01T00:00:00.000Z"),
        (-62_167_219_200_000, "0000-01-01T00:00:00.000Z"),
        (60_480_000_000, "1971-12-02T00:00:00.000Z"),
        (1_763_116_800_123, "2025-11-14T10:40:00.123Z"),
    ] {
        assert_eq!(iso_utc_ms(ms), want, "{ms}");
    }
}

#[test]
fn enums_serialise_as_the_ts_string_unions() {
    let j = |v: serde_json::Value| v.as_str().unwrap().to_string();
    let live = [
        LiveStatus::Busy,
        LiveStatus::Idle,
        LiveStatus::Waiting,
        LiveStatus::Unknown,
    ];
    let live: Vec<String> = live
        .iter()
        .map(|v| j(serde_json::to_value(v).unwrap()))
        .collect();
    assert_eq!(live, ["busy", "idle", "waiting", "unknown"]);
    let roles = [Role::Owner, Role::Peer, Role::Assistant];
    let roles: Vec<String> = roles
        .iter()
        .map(|v| j(serde_json::to_value(v).unwrap()))
        .collect();
    assert_eq!(roles, ["owner", "peer", "assistant"]);
    let states = [
        SendState::Pending,
        SendState::Held,
        SendState::Delivered,
        SendState::Expired,
        SendState::Failed,
    ];
    let states: Vec<String> = states
        .iter()
        .map(|v| j(serde_json::to_value(v).unwrap()))
        .collect();
    assert_eq!(
        states,
        ["pending", "held", "delivered", "expired", "failed"]
    );
}

#[test]
fn sessions_unrecognised_status_lists_as_unknown() {
    let db = Db::new();
    let s = db.open();
    s.upsert_session(&session()).unwrap();
    db.raw()
        .execute("UPDATE sessions SET status = 'stalled'", [])
        .unwrap();
    let rows = s.list_sessions().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, LiveStatus::Unknown);
    assert_eq!(
        s.get_session("s1").unwrap().unwrap().status,
        LiveStatus::Unknown
    );
}

#[test]
fn session_numbers_stored_as_real_by_ts_are_read_truncated() {
    // bun:sqlite binds a JS number with a fraction as REAL, and INTEGER affinity keeps it REAL.
    let db = Db::new();
    db.open();
    db.raw()
        .execute_batch(
            "INSERT INTO sessions VALUES ('s1','work','alpha','C:/x',1234.5,'interactive','idle',
               1000.7,2000.2,NULL,'C:/x/s1.jsonl',NULL);
             INSERT INTO sessions VALUES ('s2','work','beta','C:/x',7,'interactive','idle',
               1,2,3000.9,'C:/x/s2.jsonl',NULL);",
        )
        .unwrap();
    let s = db.open();
    let list = s.list_sessions_counted().unwrap();
    assert_eq!(list.unreadable, 0);
    let v = &list.sessions;
    assert_eq!(v.len(), 2);
    assert_eq!(
        (v[0].pid, v[0].started_at, v[0].updated_at),
        (1234, 1000, 2000)
    );
    assert_eq!(v[1].ended_at, Some(3000));
    let row = s.get_session("s1").unwrap().unwrap();
    assert_eq!(
        (row.pid, row.started_at, row.updated_at),
        (1234, 1000, 2000)
    );
}

#[test]
fn a_row_that_cannot_be_decoded_is_skipped_and_counted() {
    let db = Db::new();
    let s = db.open();
    s.upsert_session(&sess("s1", "t1")).unwrap();
    s.upsert_session(&SessionRow {
        name: "beta".into(),
        ..sess("s2", "t2")
    })
    .unwrap();
    s.upsert_session(&SessionRow {
        name: "gamma".into(),
        ..sess("s3", "t3")
    })
    .unwrap();
    db.raw()
        .execute_batch(
            r#"INSERT INTO status VALUES ('s1', NULL, NULL, '[]', '["leaked-text-123"]', '[]', 'r', 't');
               INSERT INTO status VALUES ('s3', NULL, NULL, '{', '[]', '[]', 'r', 't');"#,
        )
        .unwrap();
    let list = s.list_sessions_counted().unwrap();
    assert_eq!(list.unreadable, 2);
    let ids: Vec<&str> = list
        .sessions
        .iter()
        .map(|v| v.session_id.as_str())
        .collect();
    assert_eq!(ids, ["s2"]);
    // The plain list is the same good rows.
    assert_eq!(s.list_sessions().unwrap(), list.sessions);
}
