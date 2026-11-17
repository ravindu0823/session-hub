//! Port of `test/view/snapshot.test.ts`, plus the JSON key shape the tray consumes.
use hub_core::security::clean_text;
use hub_core::status::{Status, WaitingItem};
use hub_core::store::{LiveStatus, SessionRow, Store};
use hub_core::view::snapshot::read_snapshot;
use serde_json::{Value, json};
use tempfile::TempDir;

const KNOWN: [&str; 2] = ["claude", "claude-personal"];
const NOW: i64 = 1_000_000_000_000;

fn open() -> (TempDir, Store) {
    let dir = tempfile::Builder::new()
        .prefix("hub-snap-")
        .tempdir()
        .unwrap();
    let store = Store::open(&dir.path().join("index.db"), true).unwrap();
    (dir, store)
}

fn session(id: &str, name: &str) -> SessionRow {
    SessionRow {
        session_id: id.into(),
        profile: "claude".into(),
        name: name.into(),
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

fn item(id: &str, text: &str) -> WaitingItem {
    WaitingItem {
        id: Some(id.into()),
        text: text.into(),
        is_new: false,
        children: vec![],
    }
}

#[test]
fn totals_count_items_and_approvals() {
    let (_d, store) = open();
    store.upsert_session(&session("A", "a")).unwrap();
    let st = Status {
        progress_pct: Some(10),
        phase: Some("p".into()),
        remaining: vec![],
        waiting: vec![item("Q1", "one"), item("Q2", "two")],
        recommendations: vec![],
    };
    store.set_status("A", &st, "r1", "t1").unwrap();
    let mut b = session("B", "b");
    b.status = LiveStatus::Waiting;
    store.upsert_session(&b).unwrap();
    let mut c = session("C", "c");
    c.ended_at = Some(5000);
    store.upsert_session(&c).unwrap();
    let snap = read_snapshot(&store, true, &KNOWN, NOW).unwrap();
    assert_eq!(
        serde_json::to_value(snap.totals).unwrap(),
        json!({"waitingItems": 2, "needsApproval": 1, "live": 2})
    );
    assert_eq!(snap.sessions.len(), 3);
}

#[test]
fn cleans_strings() {
    let (_d, store) = open();
    store.upsert_session(&session("s1", "\x1b[31mX")).unwrap();
    let snap = read_snapshot(&store, true, &KNOWN, NOW).unwrap();
    assert_eq!(snap.sessions[0].name, clean_text("\x1b[31mX"));
    assert!(!snap.sessions[0].name.contains('\x1b'));
}

#[test]
fn cleans_status_strings() {
    let (_d, store) = open();
    store.upsert_session(&session("s1", "a")).unwrap();
    let st = Status {
        progress_pct: None,
        phase: Some("\x1b[31mph".into()),
        remaining: vec!["\x1b[1mr".into()],
        waiting: vec![WaitingItem {
            id: Some("\x1b[1mQ1".into()),
            text: "\x1b[1mt".into(),
            is_new: true,
            children: vec!["\x1b[1mc".into()],
        }],
        recommendations: vec!["\x1b[1mrec".into()],
    };
    store.set_status("s1", &st, "r1", "t1").unwrap();
    let s = &read_snapshot(&store, true, &KNOWN, NOW).unwrap().sessions[0];
    let st = s.status.as_ref().unwrap();
    assert_eq!(st.phase.as_deref(), Some(clean_text("\x1b[31mph").as_str()));
    assert!(!serde_json::to_string(st).unwrap().contains("\\u001b"));
    assert_eq!(st.remaining[0], clean_text("\x1b[1mr"));
    assert_eq!(st.waiting[0].item.children[0], clean_text("\x1b[1mc"));
    assert_eq!(st.recommendations[0], clean_text("\x1b[1mrec"));
}

#[test]
fn meta_daemon_follows_the_flag() {
    let (_d, store) = open();
    let d = |r| {
        serde_json::to_value(read_snapshot(&store, r, &KNOWN, NOW).unwrap().meta).unwrap()["daemon"]
            .clone()
    };
    assert_eq!(d(false), json!("stopped"));
    assert_eq!(d(true), json!("running"));
}

#[test]
fn meta_carries_unknown_count_version_and_stale_profiles() {
    let (_d, store) = open();
    store.set_meta("unknown_count", Some("3")).unwrap();
    store.set_meta("unknown_version", Some("9.9")).unwrap();
    store.set_meta("stale_since:claude", Some("1234")).unwrap();
    let meta = read_snapshot(&store, true, &KNOWN, NOW).unwrap().meta;
    assert_eq!(meta.unknown_count, 3);
    assert_eq!(meta.unknown_version.as_deref(), Some("9.9"));
    assert_eq!(meta.stale.len(), 1);
    assert_eq!(meta.stale[0].profile, "claude");
    assert_eq!(meta.stale[0].since, 1234);
}

#[test]
fn meta_edge_values() {
    let (_d, store) = open();
    store.set_meta("unknown_count", Some("x")).unwrap();
    store.set_meta("unknown_version", Some("")).unwrap();
    store.set_meta("stale_since:claude", Some("nope")).unwrap();
    // a profile known only from a session is checked too
    let mut s = session("s1", "a");
    s.profile = "other".into();
    store.upsert_session(&s).unwrap();
    store.set_meta("stale_since:other", Some("77")).unwrap();
    let meta = read_snapshot(&store, true, &KNOWN, NOW).unwrap().meta;
    assert_eq!(meta.unknown_count, 0);
    assert_eq!(meta.unknown_version, None);
    assert_eq!(meta.stale.len(), 1);
    assert_eq!(meta.stale[0].profile, "other");
}

#[test]
fn stray_count_for_24h_after_last_stray_else_zero() {
    let (_d, store) = open();
    let at = |t: i64| {
        read_snapshot(&store, true, &KNOWN, t)
            .unwrap()
            .meta
            .relay_stray_count
    };
    assert_eq!(at(NOW), 0);
    store.set_meta("relay_stray_count", Some("2")).unwrap();
    // counted by an older build that kept no time: not shown
    assert_eq!(at(NOW), 0);
    store
        .set_meta("relay_stray_last", Some(&(NOW - 1000).to_string()))
        .unwrap();
    assert_eq!(at(NOW), 2);
    assert_eq!(at(NOW - 1000 + 24 * 3_600_000), 2);
    assert_eq!(at(NOW - 1000 + 24 * 3_600_000 + 1), 0);
    store.set_meta("relay_stray_count", Some("x")).unwrap();
    assert_eq!(at(NOW), 0);
}

fn keys(v: &Value) -> Vec<String> {
    v.as_object().unwrap().keys().cloned().collect()
}

#[test]
fn snapshot_json_shape() {
    let (_d, store) = open();
    store.upsert_session(&session("A", "a")).unwrap();
    store.set_meta("unknown_version", Some("9.9")).unwrap();
    let v = serde_json::to_value(read_snapshot(&store, true, &KNOWN, NOW).unwrap()).unwrap();
    let mut top = keys(&v);
    top.sort();
    assert_eq!(top, ["meta", "sessions", "totals"]);
    let mut meta = keys(&v["meta"]);
    meta.sort();
    assert_eq!(
        meta,
        [
            "daemon",
            "relayStrayCount",
            "stale",
            "unknownCount",
            "unknownVersion"
        ]
    );
    let mut totals = keys(&v["totals"]);
    totals.sort();
    assert_eq!(totals, ["live", "needsApproval", "waitingItems"]);
    let order = [
        "sessionId",
        "profile",
        "name",
        "cwd",
        "pid",
        "kind",
        "startedAt",
        "updatedAt",
        "endedAt",
        "transcriptPath",
        "permissionMode",
        "state",
        "status",
        "latestReply",
        "waitingCount",
    ];
    let mut sorted = order.to_vec();
    sorted.sort();
    assert_eq!(keys(&v["sessions"][0]), sorted);
    // serde_json's map is sorted here; the struct's own serialization keeps the TS key order
    let snap = read_snapshot(&store, true, &KNOWN, NOW).unwrap();
    let text = serde_json::to_string(&snap.sessions[0]).unwrap();
    let pos: Vec<usize> = order
        .iter()
        .map(|k| text.find(&format!("\"{k}\":")).unwrap())
        .collect();
    assert!(pos.windows(2).all(|w| w[0] < w[1]), "{text}");
}

#[test]
fn meta_numbers_parse_like_js_number() {
    let (_d, store) = open();
    let count = |v: &str| {
        store.set_meta("unknown_count", Some(v)).unwrap();
        read_snapshot(&store, true, &KNOWN, NOW)
            .unwrap()
            .meta
            .unknown_count
    };
    assert_eq!(count(" 7 "), 7);
    assert_eq!(count("1e2"), 100);
    assert_eq!(count("0x10"), 16);
    assert_eq!(count("0o17"), 15);
    assert_eq!(count("0b101"), 5);
    assert_eq!(count("0x"), 0);
    assert_eq!(count("0xzz"), 0);
    assert_eq!(count("Infinity"), 0);
    assert_eq!(count("inf"), 0);
    assert_eq!(count(""), 0);
}

#[test]
fn a_store_error_is_returned_not_an_empty_snapshot() {
    let (dir, store) = open();
    store.upsert_session(&session("A", "a")).unwrap();
    // meta read fails: no such table
    rusqlite::Connection::open(dir.path().join("index.db"))
        .unwrap()
        .execute_batch("DROP TABLE meta")
        .unwrap();
    assert!(read_snapshot(&store, true, &KNOWN, NOW).is_err());
    // list_sessions fails too
    rusqlite::Connection::open(dir.path().join("index.db"))
        .unwrap()
        .execute_batch("DROP TABLE status")
        .unwrap();
    assert!(store.list_sessions().is_err());
    assert!(read_snapshot(&store, true, &KNOWN, NOW).is_err());
}

#[test]
fn an_unreadable_session_row_is_left_out_and_counted_in_meta() {
    let (dir, store) = open();
    store.upsert_session(&session("A", "a")).unwrap();
    store.upsert_session(&session("B", "b")).unwrap();
    let v = serde_json::to_value(read_snapshot(&store, true, &KNOWN, NOW).unwrap()).unwrap();
    // Zero is not serialized: the meta keys stay those of the TS snapshot.
    assert!(v["meta"].get("unreadableSessions").is_none());
    rusqlite::Connection::open(dir.path().join("index.db"))
        .unwrap()
        .execute_batch(r#"INSERT INTO status VALUES ('A', NULL, NULL, '[]', '{', '[]', 'r', 't');"#)
        .unwrap();
    let snap = read_snapshot(&store, true, &KNOWN, NOW).unwrap();
    assert_eq!(snap.sessions.len(), 1);
    assert_eq!(snap.sessions[0].session_id, "B");
    assert_eq!(snap.meta.unreadable_sessions, 1);
    assert_eq!(snap.totals.live, 1);
}
