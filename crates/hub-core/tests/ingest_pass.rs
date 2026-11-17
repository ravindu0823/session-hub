//! Port of `test/daemon/ingest-pass.test.ts`, plus status, mode and on_records checks.

use std::cell::RefCell;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use hub_core::daemon::ingest_pass::{
    IngestCtx, IngestResult, MAX_BYTES_PER_PASS, MAX_UNRECOGNIZED_TYPES, MIN_READ_BYTES,
    READ_CHUNK_BYTES, ingest_file, mask_status, safe_version,
};
use hub_core::ingest::records::Classified;
use hub_core::send::parse_js_date;
use hub_core::status::{Status, WaitingItem};
use hub_core::store::{LiveStatus, MessageRow, Role, SessionRow, Store};
use serde_json::{Value, json};
use tempfile::TempDir;

const SID: &str = "11111111-2222-4333-8444-555555555555";

struct Env {
    _dir: TempDir,
    store: Store,
    path: PathBuf,
}

fn row(path: &Path) -> SessionRow {
    SessionRow {
        session_id: SID.into(),
        profile: "claude".into(),
        name: "n".into(),
        cwd: "C:\\fixture".into(),
        pid: 1,
        kind: "interactive".into(),
        status: LiveStatus::Idle,
        started_at: 1,
        updated_at: 1,
        ended_at: None,
        transcript_path: path.to_string_lossy().into_owned(),
        permission_mode: None,
    }
}

fn env() -> Env {
    let dir = tempfile::Builder::new()
        .prefix("hub-ingest-")
        .tempdir()
        .unwrap();
    let store = Store::open(&dir.path().join("hub.db"), true).unwrap();
    let path = dir.path().join(format!("{SID}.jsonl"));
    store.upsert_session(&row(&path)).unwrap();
    Env {
        _dir: dir,
        store,
        path,
    }
}

fn no_err(e: &dyn std::error::Error) {
    panic!("on_records error: {e}");
}

fn ingest_with(env: &Env, now: i64) -> IngestResult {
    let mut ctx = IngestCtx {
        store: &env.store,
        get_row: &|_| None,
        row_changed: &|_| {},
        now: &|| now,
        on_records: None,
    };
    ingest_file(&mut ctx, SID, &env.path, &no_err).unwrap()
}

fn ingest(env: &Env) -> IngestResult {
    ingest_with(env, 1_000)
}

fn write(p: &Path, text: &str) {
    std::fs::write(p, text).unwrap();
}

fn append(p: &Path, text: &str) {
    OpenOptions::new()
        .append(true)
        .open(p)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
}

fn jsonl(values: &[Value]) -> String {
    values.iter().map(|v| format!("{v}\n")).collect()
}

fn meta(env: &Env, k: &str) -> Option<String> {
    env.store.get_meta(k).unwrap()
}

fn meta_json(env: &Env, k: &str) -> Value {
    serde_json::from_str(&meta(env, k).unwrap_or_else(|| "null".into())).unwrap()
}

fn history(env: &Env, limit: i64) -> Vec<MessageRow> {
    env.store.history(SID, limit, None).unwrap()
}

fn offset(env: &Env) -> u64 {
    env.store
        .get_tail(&env.path.to_string_lossy())
        .unwrap()
        .map_or(0, |t| t.offset)
}

fn big_line(i: usize) -> String {
    json!({
        "type": "user",
        "isSidechain": false,
        "uuid": format!("b-{i}"),
        "timestamp": "2026-11-13T08:00:00.000Z",
        "origin": { "kind": "human" },
        "message": { "role": "user", "content": format!("p{i}") },
        "pad": "x".repeat(1000),
    })
    .to_string()
}

fn user_line(uuid: &str, ts: Option<&str>) -> Value {
    let mut v = json!({
        "type": "user",
        "isSidechain": false,
        "uuid": uuid,
        "origin": { "kind": "human" },
        "message": { "role": "user", "content": uuid },
    });
    if let Some(ts) = ts {
        v["timestamp"] = json!(ts);
    }
    v
}

fn assistant_line(uuid: &str, ts: &str, text: &str) -> Value {
    json!({
        "type": "assistant",
        "isSidechain": false,
        "uuid": uuid,
        "timestamp": ts,
        "message": { "role": "assistant", "content": [{ "type": "text", "text": text }] },
    })
}

fn date(s: &str) -> i64 {
    parse_js_date(s).unwrap()
}

#[test]
fn constants_match_ts() {
    assert_eq!(READ_CHUNK_BYTES, 4 * 1024 * 1024);
    assert_eq!(MIN_READ_BYTES, 64 * 1024);
    assert_eq!(MAX_BYTES_PER_PASS, 8 * 1024 * 1024);
    assert_eq!(MAX_UNRECOGNIZED_TYPES, 10);
}

// ---------------------------------------------------------------- updatedAt

mod updated_at {
    use super::*;

    const NOW_ISO: &str = "2026-11-13T12:00:00.000Z";

    fn run(env: &Env, updated_at: i64, lines: &[Value]) -> Option<SessionRow> {
        let now = date(NOW_ISO);
        let seen: RefCell<Option<SessionRow>> = RefCell::new(None);
        let cached = SessionRow {
            updated_at,
            ..row(&env.path)
        };
        write(&env.path, &jsonl(lines));
        let get_row = |_: &str| Some(cached.clone());
        let row_changed = |r: &SessionRow| *seen.borrow_mut() = Some(r.clone());
        let mut ctx = IngestCtx {
            store: &env.store,
            get_row: &get_row,
            row_changed: &row_changed,
            now: &|| now,
            on_records: None,
        };
        ingest_file(&mut ctx, SID, &env.path, &no_err).unwrap();
        seen.into_inner()
    }

    #[test]
    fn a_new_message_dates_the_session_by_its_timestamp_not_by_now() {
        let e = env();
        let r = run(
            &e,
            date(NOW_ISO) - 3_600_000,
            &[user_line("m1", Some("2026-11-13T11:30:00.000Z"))],
        );
        assert_eq!(r.unwrap().updated_at, date("2026-11-13T11:30:00.000Z"));
        // The row was written in the same transaction.
        assert_eq!(
            e.store.get_session(SID).unwrap().unwrap().updated_at,
            date("2026-11-13T11:30:00.000Z")
        );
    }

    #[test]
    fn a_replayed_batch_of_older_messages_leaves_updated_at_alone_no_row_write() {
        let e = env();
        let r = run(
            &e,
            date(NOW_ISO) - 60_000,
            &[
                user_line("m1", Some("2026-11-12T08:00:00.000Z")),
                user_line("m2", Some("2026-11-12T09:00:00.000Z")),
            ],
        );
        assert!(r.is_none());
        assert_eq!(history(&e, 10).len(), 2);
        assert_eq!(e.store.get_session(SID).unwrap().unwrap().updated_at, 1);
    }

    #[test]
    fn a_timestamp_ahead_of_our_clock_is_capped_at_now() {
        let e = env();
        let r = run(
            &e,
            date(NOW_ISO) - 60_000,
            &[user_line("m1", Some("2027-01-01T00:00:00.000Z"))],
        );
        assert_eq!(r.unwrap().updated_at, date(NOW_ISO));
    }

    #[test]
    fn the_newest_of_the_batch_counts_in_any_order() {
        let e = env();
        let r = run(
            &e,
            1,
            &[
                user_line("m1", Some("2026-11-13T10:00:00.000Z")),
                user_line("m2", Some("2026-11-13T11:00:00.000Z")),
                user_line("m3", Some("2026-11-13T09:00:00.000Z")),
            ],
        );
        assert_eq!(r.unwrap().updated_at, date("2026-11-13T11:00:00.000Z"));
    }

    #[test]
    fn a_permission_mode_change_is_written_and_reported() {
        let e = env();
        let r = run(
            &e,
            date(NOW_ISO),
            &[json!({ "type": "permission-mode", "permissionMode": "plan" })],
        );
        let r = r.expect("row changed");
        assert_eq!(r.permission_mode.as_deref(), Some("plan"));
        assert_eq!(r.updated_at, date(NOW_ISO));
        assert_eq!(
            e.store
                .get_session(SID)
                .unwrap()
                .unwrap()
                .permission_mode
                .as_deref(),
            Some("plan")
        );
    }

    #[test]
    fn the_same_permission_mode_writes_nothing() {
        let e = env();
        let now = date(NOW_ISO);
        let seen = RefCell::new(0);
        let cached = SessionRow {
            permission_mode: Some("plan".into()),
            updated_at: now,
            ..row(&e.path)
        };
        write(
            &e.path,
            &jsonl(&[json!({ "type": "permission-mode", "permissionMode": "plan" })]),
        );
        let mut ctx = IngestCtx {
            store: &e.store,
            get_row: &|_| Some(cached.clone()),
            row_changed: &|_| *seen.borrow_mut() += 1,
            now: &|| now,
            on_records: None,
        };
        ingest_file(&mut ctx, SID, &e.path, &no_err).unwrap();
        assert_eq!(*seen.borrow(), 0);
    }
}

// ---------------------------------------------------------------- ingest_file

#[test]
fn one_call_reads_at_most_the_8_mb_budget_plus_the_floor_and_one_line_the_next_finishes() {
    let e = env();
    let total = 12_500;
    let text: String = (0..total).map(|i| format!("{}\n", big_line(i))).collect();
    write(&e.path, &text);
    let size = std::fs::metadata(&e.path).unwrap().len();
    let line_len = (big_line(0).len() + 1) as u64;
    assert!(size > 12 * 1024 * 1024);

    let r1 = ingest(&e);
    let off1 = offset(&e);
    assert!(r1.more);
    assert!(off1 <= MAX_BYTES_PER_PASS + MIN_READ_BYTES + line_len);
    assert!(off1 > MAX_BYTES_PER_PASS - line_len);

    let r2 = ingest(&e);
    assert!(!r2.more);
    assert_eq!(offset(&e), size);
    assert_eq!(r1.lines + r2.lines, total);
    assert_eq!(history(&e, total as i64 + 1).len(), total);
}

#[test]
fn unknown_version_comes_from_the_newest_unknown_record_that_has_a_version() {
    let e = env();
    write(
        &e.path,
        &jsonl(&[
            json!({ "type": "user", "message": null, "version": "2.1.400" }),
            json!({ "type": "assistant", "message": "s", "version": "2.1.401" }),
            json!({ "type": "user", "message": { "content": 1 } }),
        ]),
    );
    let r = ingest(&e);
    assert_eq!(r.unknown, 3);
    assert_eq!(meta(&e, "unknown_count").as_deref(), Some("3"));
    assert_eq!(meta(&e, "unknown_version").as_deref(), Some("2.1.401"));
    append(
        &e.path,
        &jsonl(&[json!({ "type": "user", "version": "2.2.0" })]),
    );
    ingest(&e);
    assert_eq!(meta(&e, "unknown_count").as_deref(), Some("4"));
    assert_eq!(meta(&e, "unknown_version").as_deref(), Some("2.2.0"));
}

#[test]
fn unknown_without_any_version_stores_an_empty_version_once() {
    let e = env();
    write(
        &e.path,
        &jsonl(&[json!({ "type": "user", "message": null })]),
    );
    ingest(&e);
    assert_eq!(meta(&e, "unknown_count").as_deref(), Some("1"));
    assert_eq!(meta(&e, "unknown_version").as_deref(), Some(""));
    e.store.set_meta("unknown_version", Some("2.0.0")).unwrap();
    append(
        &e.path,
        &jsonl(&[json!({ "type": "user", "message": null })]),
    );
    ingest(&e);
    // An unknown record without a version leaves a stored one alone.
    assert_eq!(meta(&e, "unknown_version").as_deref(), Some("2.0.0"));
}

#[test]
fn unknown_version_is_capped_32_safe_chars_else_invalid() {
    let e = env();
    write(&e.path, "");
    let v32 = "v".repeat(32);
    let v33 = "v".repeat(33);
    let cases: [(&str, &str); 6] = [
        ("2.1.323+build_7-rc", "2.1.323+build_7-rc"),
        (&v32, &v32),
        (&v33, "invalid"),
        ("2.1 with spaces", "invalid"),
        ("fixture\nline", "invalid"),
        ("2.1.0\u{e9}", "invalid"),
    ];
    for (version, stored) in cases {
        append(
            &e.path,
            &jsonl(&[json!({ "type": "user", "message": null, "version": version })]),
        );
        ingest(&e);
        assert_eq!(
            meta(&e, "unknown_version").as_deref(),
            Some(stored),
            "{version:?}"
        );
    }
}

#[test]
fn safe_version_matches_the_ts_pattern() {
    assert_eq!(safe_version("1.0.0"), "1.0.0");
    assert_eq!(safe_version(""), "invalid");
    assert_eq!(safe_version("1.0\n"), "invalid");
    assert_eq!(safe_version("１.0"), "invalid");
}

#[test]
fn unrecognised_types_are_counted_apart_and_never_raise_unknown_count() {
    let e = env();
    let types = [
        "pr-link-v2",
        "frame.link",
        "cost_state",
        "brand-new-type",
        "pr-link-v2",
    ];
    let lines: Vec<Value> = types
        .iter()
        .map(|t| json!({ "type": t, "version": "2.3.0", "url": "fixture x" }))
        .collect();
    write(&e.path, &jsonl(&lines));
    let r = ingest(&e);
    assert_eq!(r.unknown, 0);
    assert_eq!(r.unrecognized, 5);
    assert_eq!(meta(&e, "unknown_count"), None);
    assert_eq!(meta(&e, "unknown_version"), None);
    assert_eq!(meta(&e, "unrecognized_count").as_deref(), Some("5"));
    assert_eq!(
        meta(&e, "unrecognized_types").as_deref(),
        Some(r#"["pr-link-v2","frame.link","cost_state","brand-new-type"]"#)
    );
    append(&e.path, &jsonl(&[json!({ "type": "another-one" })]));
    ingest(&e);
    assert_eq!(meta(&e, "unrecognized_count").as_deref(), Some("6"));
    assert_eq!(
        meta_json(&e, "unrecognized_types")
            .as_array()
            .unwrap()
            .len(),
        5
    );
}

#[test]
fn unrecognized_types_keeps_at_most_10_safe_names_unsafe_names_are_counted_not_listed() {
    let e = env();
    let x41 = "x".repeat(41);
    let unsafe_names = ["", "has space", &x41, "fixture\nline", "a/b", "é-type"];
    let safe: Vec<String> = (0..12).map(|i| format!("type-{i}")).collect();
    let lines: Vec<Value> = unsafe_names
        .iter()
        .map(|t| t.to_string())
        .chain(safe.iter().cloned())
        .map(|t| json!({ "type": t }))
        .collect();
    write(&e.path, &jsonl(&lines));
    ingest(&e);
    assert_eq!(
        meta(&e, "unrecognized_count"),
        Some((unsafe_names.len() + safe.len()).to_string())
    );
    assert_eq!(meta_json(&e, "unrecognized_types"), json!(safe[..10]));
}

#[test]
fn a_40_character_name_is_listed() {
    let e = env();
    write(&e.path, &jsonl(&[json!({ "type": "x".repeat(40) })]));
    ingest(&e);
    assert_eq!(meta_json(&e, "unrecognized_types"), json!(["x".repeat(40)]));
}

#[test]
fn a_corrupt_unrecognized_meta_is_replaced_not_trusted() {
    let e = env();
    e.store.set_meta("unrecognized_count", Some("-7")).unwrap();
    e.store
        .set_meta("unrecognized_types", Some(r#"{"not":"a list"}"#))
        .unwrap();
    write(&e.path, &jsonl(&[json!({ "type": "new-type" })]));
    ingest(&e);
    assert_eq!(meta(&e, "unrecognized_count").as_deref(), Some("1"));
    assert_eq!(meta_json(&e, "unrecognized_types"), json!(["new-type"]));
    e.store
        .set_meta("unrecognized_types", Some(r#"["ok-type", "bad name", 5]"#))
        .unwrap();
    append(&e.path, &jsonl(&[json!({ "type": "next-type" })]));
    ingest(&e);
    assert_eq!(
        meta_json(&e, "unrecognized_types"),
        json!(["ok-type", "next-type"])
    );
    // Not JSON at all: started over.
    e.store
        .set_meta("unrecognized_types", Some("not json"))
        .unwrap();
    e.store.set_meta("unrecognized_count", Some("1.5")).unwrap();
    append(&e.path, &jsonl(&[json!({ "type": "third-type" })]));
    ingest(&e);
    assert_eq!(meta_json(&e, "unrecognized_types"), json!(["third-type"]));
    assert_eq!(meta(&e, "unrecognized_count").as_deref(), Some("1"));
}

#[test]
fn a_stored_count_is_read_like_js_number() {
    let e = env();
    write(&e.path, "");
    for (stored, next) in [(" 12 ", "13"), ("", "1"), ("1e3", "1001"), ("abc", "1")] {
        e.store.set_meta("unknown_count", Some(stored)).unwrap();
        append(
            &e.path,
            &jsonl(&[json!({ "type": "user", "message": null })]),
        );
        ingest(&e);
        assert_eq!(
            meta(&e, "unknown_count").as_deref(),
            Some(next),
            "{stored:?}"
        );
    }
}

#[test]
fn a_long_line_with_a_tiny_budget_left_is_read_in_large_chunks_not_crawled() {
    // Lines of 1 MiB - 1 byte: two 4 MiB reads take 4 lines each and leave 8 bytes of budget.
    let e = env();
    let line_len = 1024 * 1024 - 1;
    let prefix = r#"{"type":"queue-operation","pad":""#;
    let suffix = r#""}"#;
    let line = format!(
        "{prefix}{}{suffix}\n",
        "x".repeat(line_len - 1 - prefix.len() - suffix.len())
    );
    assert_eq!(line.len(), line_len);
    write(&e.path, &line.repeat(10));
    let t0 = Instant::now();
    let r = ingest(&e);
    let ms = t0.elapsed().as_millis();
    assert_eq!(r.lines, 9);
    assert!(r.more);
    assert_eq!(offset(&e), 9 * line_len as u64);
    // 8-byte reads of the 9th line would take ~131k reads; 64 KiB reads take 16.
    assert!(ms < 1_500, "{ms} ms");
}

#[test]
fn nothing_new_writes_nothing_and_a_partial_last_line_waits() {
    let e = env();
    write(
        &e.path,
        &jsonl(&[user_line("a", Some("2026-11-13T08:00:00.000Z"))]),
    );
    assert_eq!(ingest(&e).lines, 1);
    let r = ingest(&e);
    assert_eq!((r.lines, r.messages, r.more), (0, 0, false));
    let half = user_line("b", Some("2026-11-13T08:00:01.000Z")).to_string();
    append(&e.path, &half[..10]);
    assert_eq!(ingest(&e).lines, 0);
    append(&e.path, &format!("{}\n", &half[10..]));
    assert_eq!(ingest(&e).lines, 1);
    assert_eq!(history(&e, 10).len(), 2);
}

#[test]
fn a_truncated_file_is_read_again_from_the_start() {
    let e = env();
    write(
        &e.path,
        &jsonl(&[
            user_line("a", Some("2026-11-13T08:00:00.000Z")),
            user_line("b", Some("2026-11-13T08:00:01.000Z")),
        ]),
    );
    ingest(&e);
    write(
        &e.path,
        &jsonl(&[user_line("c", Some("2026-11-13T08:00:02.000Z"))]),
    );
    let r = ingest(&e);
    assert_eq!(r.lines, 1);
    let uuids: Vec<String> = history(&e, 10).into_iter().map(|m| m.uuid).collect();
    assert_eq!(uuids, ["a", "b", "c"]);
}

#[test]
fn a_missing_file_is_an_error() {
    let e = env();
    let mut ctx = IngestCtx {
        store: &e.store,
        get_row: &|_| None,
        row_changed: &|_| {},
        now: &|| 1,
        on_records: None,
    };
    assert!(ingest_file(&mut ctx, SID, &e.path, &no_err).is_err());
}

// ---------------------------------------------------------------- timestamps

#[test]
fn an_offset_timestamp_is_converted_a_missing_one_takes_the_batch_time() {
    let e = env();
    write(
        &e.path,
        &jsonl(&[
            user_line("a", Some("2026-11-13T10:00:00+02:00")),
            user_line("b", None),
            user_line("c", Some("garbage")),
        ]),
    );
    ingest_with(&e, date("2026-11-14T00:00:00.000Z"));
    let rows = history(&e, 10);
    let ts = |u: &str| rows.iter().find(|m| m.uuid == u).unwrap().ts.clone();
    assert_eq!(ts("a"), "2026-11-13T08:00:00.000Z");
    assert_eq!(ts("b"), "2026-11-13T08:00:00.000Z");
    assert_eq!(ts("c"), "2026-11-13T08:00:00.000Z");
}

#[test]
fn a_batch_without_any_timestamp_takes_now() {
    let e = env();
    write(&e.path, &jsonl(&[user_line("a", None)]));
    ingest_with(&e, date("2026-11-14T00:00:00.000Z"));
    assert_eq!(history(&e, 10)[0].ts, "2026-11-14T00:00:00.000Z");
}

// ---------------------------------------------------------------- masking

#[test]
fn a_peer_sender_name_that_carries_a_secret_is_masked_before_it_is_stored() {
    let e = env();
    let name = format!("ghp_{}", "Q".repeat(36));
    write(
        &e.path,
        &jsonl(&[json!({
            "type": "attachment",
            "isSidechain": false,
            "uuid": "pn-1",
            "timestamp": "2026-11-13T08:00:00.000Z",
            "attachment": {
                "type": "queued_command",
                "prompt": "x",
                "origin": { "kind": "peer", "name": name, "body": "card 4111111111111111", "verifiedLocal": true },
            },
        })]),
    );
    ingest(&e);
    let rows = history(&e, 10);
    assert_eq!(rows[0].role, Role::Peer);
    assert_eq!(rows[0].from_name.as_deref(), Some("[masked:github]"));
    assert_eq!(rows[0].text, "card [masked:card]");
}

#[test]
fn an_owner_message_queued_while_busy_is_stored_once_as_an_owner_message_masked() {
    let e = env();
    write(
        &e.path,
        &jsonl(&[json!({
            "type": "attachment",
            "isSidechain": false,
            "uuid": "q-1",
            "timestamp": "2026-11-13T08:00:00.000Z",
            "attachment": {
                "type": "queued_command",
                "prompt": "Q1 yes, card 4111111111111111",
                "commandMode": "prompt",
                "origin": { "kind": "human" },
                "humanTurn": true,
            },
        })]),
    );
    ingest(&e);
    ingest(&e);
    let rows = history(&e, 10);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].role, Role::Owner);
    assert_eq!(rows[0].from_name, None);
    assert_eq!(rows[0].text, "Q1 yes, card [masked:card]");
}

// ---------------------------------------------------------------- status

const STATUS_REPLY: &str = "**Progress: about 40% overall. Phase: build ghp_QQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQ.**\n\n### What's remaining\n1. ship card 4111111111111111\n\n### Waiting on you\n- Q1: approve card 4111111111111111?\n";

#[test]
fn status_is_parsed_from_the_newest_reply_before_masking_then_masked() {
    let e = env();
    write(
        &e.path,
        &jsonl(&[
            assistant_line("r1", "2026-11-13T08:00:00.000Z", "no status here"),
            assistant_line("r2", "2026-11-13T08:00:01+01:00", STATUS_REPLY),
        ]),
    );
    ingest(&e);
    let views = e.store.list_sessions().unwrap();
    let v = views.iter().find(|v| v.session_id == SID).unwrap();
    let st = v.status.as_ref().expect("status stored");
    assert_eq!(st.reply_uuid, "r2");
    // Dated by its reply, in the normalised ISO form.
    assert_eq!(st.reply_ts, "2026-11-13T07:00:01.000Z");
    assert_eq!(st.progress_pct, Some(40));
    let all = serde_json::to_string(st).unwrap();
    assert!(!all.contains("4111111111111111"), "{all}");
    assert!(!all.contains("ghp_QQQQ"), "{all}");
    assert!(all.contains("[masked:card]"), "{all}");
    let lr = v.latest_reply.as_ref().unwrap();
    assert_eq!(
        (lr.reply_uuid.as_str(), lr.reply_ts.as_str(), lr.has_status),
        ("r2", "2026-11-13T07:00:01.000Z", true)
    );
    // Stored assistant text is masked too.
    let rows = history(&e, 10);
    assert!(rows.iter().all(|m| !m.text.contains("4111111111111111")));
}

#[test]
fn a_newest_reply_without_status_keeps_the_old_status_and_marks_the_latest_reply() {
    let e = env();
    write(
        &e.path,
        &jsonl(&[assistant_line(
            "r1",
            "2026-11-13T08:00:00.000Z",
            STATUS_REPLY,
        )]),
    );
    ingest(&e);
    append(
        &e.path,
        &jsonl(&[assistant_line("r2", "2026-11-13T09:00:00.000Z", "plain")]),
    );
    ingest(&e);
    let views = e.store.list_sessions().unwrap();
    let v = views.iter().find(|v| v.session_id == SID).unwrap();
    assert_eq!(v.status.as_ref().unwrap().reply_uuid, "r1");
    let lr = v.latest_reply.as_ref().unwrap();
    assert_eq!((lr.reply_uuid.as_str(), lr.has_status), ("r2", false));
}

#[test]
fn mask_status_masks_every_string_and_keeps_numbers_and_flags() {
    let card = "4111111111111111";
    let st = Status {
        progress_pct: Some(7),
        phase: Some(format!("p {card}")),
        remaining: vec![format!("r {card}")],
        waiting: vec![
            WaitingItem {
                id: Some(card.into()),
                text: format!("w {card}"),
                is_new: true,
                children: vec![format!("c {card}")],
            },
            WaitingItem {
                id: None,
                text: "plain".into(),
                is_new: false,
                children: vec![],
            },
        ],
        recommendations: vec![format!("x {card}")],
    };
    let m = mask_status(&st);
    assert_eq!(m.progress_pct, Some(7));
    assert_eq!(m.phase.as_deref(), Some("p [masked:card]"));
    assert_eq!(m.remaining, ["r [masked:card]"]);
    assert_eq!(m.waiting[0].id.as_deref(), Some("[masked:card]"));
    assert_eq!(m.waiting[0].text, "w [masked:card]");
    assert!(m.waiting[0].is_new);
    assert_eq!(m.waiting[0].children, ["c [masked:card]"]);
    assert_eq!(m.waiting[1], st.waiting[1]);
    assert_eq!(m.recommendations, ["x [masked:card]"]);
    let none = Status { phase: None, ..st };
    assert_eq!(mask_status(&none).phase, None);
}

// ---------------------------------------------------------------- on_records

#[test]
fn on_records_gets_each_committed_batch_and_a_panic_is_reported_not_raised() {
    let e = env();
    write(
        &e.path,
        &jsonl(&[
            user_line("a", Some("2026-11-13T08:00:00.000Z")),
            json!({ "type": "api-request", "content": "never" }),
        ]),
    );
    let calls: RefCell<Vec<(String, usize, bool)>> = RefCell::new(vec![]);
    let mut hook = |store: &Store, sid: &str, recs: &[Classified]| {
        // The batch is committed before the hook runs.
        let committed = !store.history(sid, 10, None).unwrap().is_empty();
        calls
            .borrow_mut()
            .push((sid.to_string(), recs.len(), committed));
        panic!("hook failed");
    };
    let errors = RefCell::new(Vec::<String>::new());
    let on_err = |err: &dyn std::error::Error| errors.borrow_mut().push(err.to_string());
    let mut ctx = IngestCtx {
        store: &e.store,
        get_row: &|_| None,
        row_changed: &|_| {},
        now: &|| 1_000,
        on_records: Some(&mut hook),
    };
    let r = ingest_file(&mut ctx, SID, &e.path, &on_err).unwrap();
    assert_eq!(r.lines, 2);
    assert_eq!(*calls.borrow(), [(SID.to_string(), 2, true)]);
    // The panic text is not passed on: it could quote a record.
    assert_eq!(*errors.borrow(), ["on_records panicked"]);
    // Nothing new: the hook is not called again.
    let r = ingest_file(&mut ctx, SID, &e.path, &on_err).unwrap();
    assert_eq!(r.lines, 0);
    assert_eq!(calls.borrow().len(), 1);
}

#[test]
fn a_first_index_reads_only_the_last_first_index_bytes() {
    use hub_core::config::FIRST_INDEX_BYTES;
    let e = env();
    let text: String = (0..21_000).map(|i| format!("{}\n", big_line(i))).collect();
    assert!(text.len() as u64 > FIRST_INDEX_BYTES);
    write(&e.path, &text);
    let mut total = 0;
    loop {
        let r = ingest(&e);
        total += r.lines;
        if !r.more {
            break;
        }
    }
    assert!(total < 21_000);
    // Exactly the complete lines that start at or after size - FIRST_INDEX_BYTES.
    let cut = text.len() - FIRST_INDEX_BYTES as usize;
    let mut starts = vec![0usize];
    starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
    starts.pop(); // the end of the file
    let expected = starts.iter().filter(|&&s| s >= cut).count();
    assert_eq!(total, expected);
}
