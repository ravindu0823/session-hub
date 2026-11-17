//! Port of `test/daemon/log.test.ts`, plus the UTF-16 clip and "no message text in the log" checks.

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use hub_core::daemon::ingest_pass::{IngestCtx, ingest_file};
use hub_core::daemon::log::{LOG_MAX_BYTES, LogValue, create_logger, err_fields};
use hub_core::ingest::records::Classified;
use hub_core::store::{LiveStatus, SessionRow, Store};
use serde_json::{Map, Value};
use tempfile::TempDir;

fn tmp() -> TempDir {
    tempfile::Builder::new()
        .prefix("hub-log-")
        .tempdir()
        .unwrap()
}

fn lines(file: &Path) -> Vec<Map<String, Value>> {
    std::fs::read_to_string(file)
        .unwrap()
        .split('\n')
        .filter(|l| !l.is_empty())
        .map(|l| match serde_json::from_str(l).unwrap() {
            Value::Object(m) => m,
            other => panic!("not an object: {other}"),
        })
        .collect()
}

fn s(v: &str) -> LogValue {
    LogValue::Str(v.to_string())
}

#[test]
fn writes_json_lines_with_ts_level_event_and_fields() {
    let d = tmp();
    let log = create_logger(&d.path().join("run"), LOG_MAX_BYTES, "daemon.log");
    log.info(
        "start",
        vec![
            ("n", LogValue::Num(1.0)),
            ("ok", LogValue::Bool(true)),
            ("id", s("abc")),
        ],
    );
    log.warn("slow", vec![("ms", LogValue::Num(5.0))]);
    log.error("boom", vec![]);
    let got = lines(&d.path().join("run").join("daemon.log"));
    let le: Vec<(&str, &str)> = got
        .iter()
        .map(|l| (l["level"].as_str().unwrap(), l["event"].as_str().unwrap()))
        .collect();
    assert_eq!(le, [("info", "start"), ("warn", "slow"), ("error", "boom")]);
    assert_eq!(got[0]["n"], Value::from(1));
    assert_eq!(got[0]["ok"], Value::Bool(true));
    assert_eq!(got[0]["id"], Value::from("abc"));
    assert!(got[0]["ts"].is_string());
}

#[test]
fn line_layout_matches_js_json_stringify() {
    let d = tmp();
    let log = create_logger(d.path(), LOG_MAX_BYTES, "daemon.log");
    log.info(
        "e",
        vec![
            ("n", LogValue::Num(3.0)),
            ("f", LogValue::Num(0.5)),
            ("neg0", LogValue::Num(-0.0)),
            ("nan", LogValue::Num(f64::NAN)),
            ("q", s("a\"b\\c\nd")),
        ],
    );
    let text = std::fs::read_to_string(d.path().join("daemon.log")).unwrap();
    let ts = lines(&d.path().join("daemon.log"))[0]["ts"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        text,
        format!(
            "{{\"n\":3,\"f\":0.5,\"neg0\":0,\"nan\":null,\"q\":\"a\\\"b\\\\c\\nd\",\"ts\":\"{ts}\",\"level\":\"info\",\"event\":\"e\"}}\n"
        )
    );
    // ISO UTC with milliseconds, like Date.toISOString.
    assert_eq!(ts.len(), 24);
    assert!(ts.ends_with('Z') && ts.as_bytes()[10] == b'T' && ts.as_bytes()[19] == b'.');
}

#[test]
fn another_file_name_in_the_same_folder_relay_log() {
    let d = tmp();
    let log = create_logger(d.path(), LOG_MAX_BYTES, "relay.log");
    log.info("relay", vec![("sendId", LogValue::Num(1.0))]);
    assert!(!d.path().join("daemon.log").exists());
    let got = lines(&d.path().join("relay.log"));
    assert_eq!(got[0]["event"], Value::from("relay"));
    assert_eq!(got[0]["sendId"], Value::from(1));
}

#[test]
fn fields_cannot_override_ts_level_or_event() {
    let d = tmp();
    let log = create_logger(d.path(), LOG_MAX_BYTES, "daemon.log");
    log.info(
        "real",
        vec![("ts", s("x")), ("level", s("error")), ("event", s("fake"))],
    );
    let text = std::fs::read_to_string(d.path().join("daemon.log")).unwrap();
    let got = lines(&d.path().join("daemon.log"));
    assert_eq!(got[0]["level"], Value::from("info"));
    assert_eq!(got[0]["event"], Value::from("real"));
    assert_ne!(got[0]["ts"], Value::from("x"));
    // Each key appears once (a JS object keeps one), in the first-set position.
    assert_eq!(text.matches("\"level\"").count(), 1);
    assert!(text.starts_with("{\"ts\":"));
}

#[test]
fn a_repeated_field_keeps_one_key_with_the_last_value() {
    let d = tmp();
    let log = create_logger(d.path(), LOG_MAX_BYTES, "daemon.log");
    log.info("e", vec![("a", s("1")), ("b", s("2")), ("a", s("3"))]);
    let text = std::fs::read_to_string(d.path().join("daemon.log")).unwrap();
    assert!(
        text.starts_with("{\"a\":\"3\",\"b\":\"2\",\"ts\":"),
        "{text}"
    );
}

#[test]
fn string_fields_are_truncated_to_200_chars() {
    let d = tmp();
    let log = create_logger(d.path(), LOG_MAX_BYTES, "daemon.log");
    log.info("long", vec![("s", s(&"a".repeat(500)))]);
    let got = lines(&d.path().join("daemon.log"));
    assert_eq!(got[0]["s"].as_str().unwrap().len(), 200);
}

#[test]
fn the_event_name_is_truncated_too() {
    let d = tmp();
    let log = create_logger(d.path(), LOG_MAX_BYTES, "daemon.log");
    log.info(&"e".repeat(300), vec![]);
    let got = lines(&d.path().join("daemon.log"));
    assert_eq!(got[0]["event"].as_str().unwrap().len(), 200);
}

#[test]
fn log_clip_utf16() {
    // 199 ASCII units + an emoji (2 units) is 201 units: the emoji cannot be split, so it goes.
    let d = tmp();
    let log = create_logger(d.path(), LOG_MAX_BYTES, "daemon.log");
    let prefix = "a".repeat(199);
    log.info("clip", vec![("s", s(&format!("{prefix}😀")))]);
    // 198 + emoji is exactly 200 units and stays whole; CJK counts one unit each.
    log.info("clip", vec![("s", s(&format!("{}😀", "a".repeat(198))))]);
    log.info("clip", vec![("s", s(&"漢".repeat(250)))]);
    let got = lines(&d.path().join("daemon.log"));
    assert_eq!(got[0]["s"].as_str().unwrap(), prefix);
    assert_eq!(
        got[1]["s"].as_str().unwrap(),
        format!("{}😀", "a".repeat(198))
    );
    assert_eq!(got[2]["s"].as_str().unwrap(), "漢".repeat(200));
}

#[test]
fn rotates_at_the_size_limit_and_keeps_3_files() {
    let d = tmp();
    let log = create_logger(d.path(), 1000, "daemon.log");
    for i in 0..100 {
        log.info(
            "fill",
            vec![
                ("i", LogValue::Num(f64::from(i))),
                ("pad", s(&"x".repeat(100))),
            ],
        );
    }
    for f in ["daemon.log", "daemon.log.1", "daemon.log.2"] {
        let p = d.path().join(f);
        assert!(p.exists(), "{f}");
        assert!(std::fs::metadata(&p).unwrap().len() <= 1000, "{f}");
    }
    assert!(!d.path().join("daemon.log.3").exists());
    let last = lines(&d.path().join("daemon.log"));
    assert_eq!(last.last().unwrap()["i"], Value::from(99));
}

#[test]
fn an_existing_file_counts_toward_the_limit() {
    let d = tmp();
    std::fs::write(d.path().join("daemon.log"), "x".repeat(990)).unwrap();
    let log = create_logger(d.path(), 1000, "daemon.log");
    log.info("next", vec![]);
    assert_eq!(
        std::fs::read_to_string(d.path().join("daemon.log.1")).unwrap(),
        "x".repeat(990)
    );
    assert_eq!(lines(&d.path().join("daemon.log")).len(), 1);
}

#[test]
fn a_logger_that_cannot_write_never_panics() {
    let d = tmp();
    // The "folder" is a file: creating it and every write fail silently.
    let file = d.path().join("blocker");
    std::fs::write(&file, "x").unwrap();
    let log = create_logger(&file, LOG_MAX_BYTES, "daemon.log");
    log.info("lost", vec![("n", LogValue::Num(1.0))]);
    log.error("lost", vec![]);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "x");
}

#[derive(Debug)]
struct NamedError(String);
impl std::fmt::Display for NamedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for NamedError {}

#[test]
fn err_fields_name_and_truncated_message_only() {
    let f = err_fields(&NamedError("m".repeat(300)));
    assert_eq!(f.len(), 2);
    assert_eq!(f[0].0, "err");
    assert_eq!(f[1].0, "msg");
    let LogValue::Str(msg) = &f[1].1 else {
        panic!("msg is a string")
    };
    assert_eq!(msg.len(), 200);
    let f = err_fields(&NamedError("plain".into()));
    assert_eq!(f, vec![("err", s("Error")), ("msg", s("plain"))]);
    let f = err_fields(&std::io::Error::new(std::io::ErrorKind::NotFound, "gone"));
    assert_eq!(f, vec![("err", s("Error")), ("msg", s("gone"))]);
}

// ---------------------------------------------------------------- no message text in the log

/// Every string under a key that carries message text, anywhere in the record.
fn texts(v: &Value, out: &mut BTreeSet<String>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                if let Value::String(t) = x
                    && matches!(
                        k.as_str(),
                        "text" | "body" | "content" | "prompt" | "blob" | "shape" | "delta"
                    )
                    && t.chars().count() >= 4
                {
                    out.insert(t.clone());
                }
                texts(x, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| texts(x, out)),
        _ => {}
    }
}

fn fixture_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(common::fixture("records"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    files.sort();
    files
}

#[test]
fn log_never_writes_message_text() {
    let d = tmp();
    let store = Store::open(&d.path().join("hub.db"), true).unwrap();
    store
        .upsert_session(&SessionRow {
            session_id: "s1".into(),
            profile: "claude".into(),
            name: "n".into(),
            cwd: "C:\\fixture".into(),
            pid: 1,
            kind: "interactive".into(),
            status: LiveStatus::Idle,
            started_at: 1,
            updated_at: 1,
            ended_at: None,
            transcript_path: String::new(),
            permission_mode: None,
        })
        .unwrap();
    let log = create_logger(&d.path().join("run"), LOG_MAX_BYTES, "daemon.log");
    let mut secrets = BTreeSet::new();
    let files = fixture_files();
    assert!(files.len() >= 5, "fixtures found: {files:?}");
    for (i, f) in files.iter().enumerate() {
        let text = std::fs::read_to_string(f).unwrap();
        for line in text.lines() {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                texts(&v, &mut secrets);
            }
        }
        // A copy, so the tail rows never point into the repo.
        let path = d.path().join(format!("{i}.jsonl"));
        std::fs::write(&path, &text).unwrap();
        // The hook fails with the record's own text in its panic message.
        let mut on_records = |_: &Store, _: &str, records: &[Classified]| {
            let t = records
                .iter()
                .find_map(|r| match r {
                    Classified::OwnerPrompt { text, .. }
                    | Classified::AssistantText { text, .. } => Some(text.clone()),
                    Classified::PeerMessage { body, .. } => Some(body.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            panic!("hook failed on {t}");
        };
        let mut ctx = IngestCtx {
            store: &store,
            get_row: &|_| None,
            row_changed: &|_| {},
            now: &|| 1_000,
            on_records: Some(&mut on_records),
        };
        let on_err = |e: &dyn std::error::Error| {
            let mut f = vec![("session", s("s1"))];
            f.extend(err_fields(e));
            log.warn("on_records_failed", f);
        };
        let r = ingest_file(&mut ctx, "s1", &path, &on_err).unwrap();
        log.info(
            "ingested",
            vec![
                ("lines", LogValue::Num(r.lines as f64)),
                ("messages", LogValue::Num(r.messages as f64)),
            ],
        );
    }
    assert!(secrets.len() >= 5, "fixture texts found: {secrets:?}");
    let logged = std::fs::read_to_string(d.path().join("run").join("daemon.log")).unwrap();
    assert!(logged.contains("on_records_failed"), "{logged}");
    for t in &secrets {
        assert!(
            !logged.contains(t.as_str()),
            "log contains fixture text {t:?}"
        );
    }
}
