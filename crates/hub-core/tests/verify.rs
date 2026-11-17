//! Port of `test/send/verify.test.ts` (checkDelivery, deliveryChecker, expireSends; the
//! Store.transitionSend block is covered in tests/store.rs), plus parity and date-parse checks.

use std::sync::Arc;

use hub_core::config::{DELIVERY_WINDOW_MS, EVAL_SESSION_PREFIX, MESSAGE_HEADER, RELAY_NAME};
use hub_core::daemon::log::{LogValue, MemoryLogger};
use hub_core::ingest::records::{Classified, classify};
use hub_core::send::{
    HEADERLESS_DETAIL, REFLOWED_DETAIL, build_message, check_delivery, delivery_checker,
    delivery_match, expire_sends, normalize, parse_js_date, sha256_hex,
};
use hub_core::store::{LiveStatus, SendRow, SendState, SessionRow, Store, iso_utc_ms};
use rusqlite::{Connection, OpenFlags};
use tempfile::TempDir;

/// 2026-11-13T10:00:00.000Z
const T0: i64 = 1_794_564_000_000;
const WINDOW: i64 = DELIVERY_WINDOW_MS as i64;

fn iso(ms: i64) -> String {
    iso_utc_ms(ms)
}

fn send_for(text: &str) -> SendRow {
    SendRow {
        id: Some(1),
        session_id: "s1".into(),
        profile: "claude".into(),
        name: "alpha".into(),
        sha256: sha256_hex(&normalize(&build_message(text))),
        created_at: T0,
        state: SendState::Pending,
        detail: None,
    }
}

fn send_with(text: &str, f: impl FnOnce(&mut SendRow)) -> SendRow {
    let mut s = send_for(text);
    f(&mut s);
    s
}

fn peer_as(body: &str, ts: i64, from: &str, uuid: &str) -> Classified {
    Classified::PeerMessage {
        uuid: uuid.into(),
        ts: iso(ts),
        from_name: from.into(),
        from_mode: "bypass".into(),
        body: body.into(),
    }
}

fn peer(body: &str, ts: i64) -> Classified {
    peer_as(body, ts, RELAY_NAME, &format!("p-{ts}"))
}

fn held_by(ts: i64, name: &str) -> Classified {
    Classified::HeldNotice {
        uuid: format!("h-{ts}"),
        ts: iso(ts),
        peer_name: name.into(),
    }
}

fn held(ts: i64) -> Classified {
    held_by(ts, RELAY_NAME)
}

#[test]
fn build_message_is_header_blank_line_text() {
    assert_eq!(
        build_message("hi\nthere"),
        format!("{MESSAGE_HEADER}\n\nhi\nthere")
    );
}

#[test]
fn normalize_turns_crlf_into_lf_and_trims_trailing_whitespace_only() {
    assert_eq!(normalize("  a\r\nb \r\n\r\n \t"), "  a\nb");
    assert_eq!(normalize("x"), "x");
    // JS trimEnd set: NBSP, BOM, line/paragraph separators, ideographic space; a lone CR too.
    assert_eq!(normalize("a\u{A0}\u{FEFF}\u{2028}\u{2029}\u{3000}\r"), "a");
    // Leading whitespace and a lone CR inside stay.
    assert_eq!(normalize("\u{FEFF}a\rb"), "\u{FEFF}a\rb");
}

#[test]
fn sha256_hex_is_hex_sha256_of_utf8() {
    // `createHash("sha256").update("é😀", "utf8").digest("hex")` in Bun.
    assert_eq!(
        sha256_hex("é😀"),
        "1184d1f608158eea09d297565575892231550c403aaa913008d867a97cfd5c76"
    );
    assert_eq!(
        sha256_hex(""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn sha256_matches_ts() {
    // Computed with the TS `sha256Hex(normalize(buildMessage("hi\r\n")))`.
    assert_eq!(
        sha256_hex(&normalize(&build_message("hi\r\n"))),
        "44066cc3e795d8698d5108c7e701121ef7217dad756a9c168d30a30cfa796891"
    );
}

const TEXT: &str = "yes, go ahead 👍️ \"quoted\" `code`\nsecond line";

fn body() -> String {
    build_message(TEXT)
}

#[test]
fn delivered_on_the_exact_body_from_the_relay_after_created_at() {
    assert_eq!(
        check_delivery(&send_for(TEXT), &[peer(&body(), T0 + 5000)], T0 + 6000),
        SendState::Delivered
    );
}

#[test]
fn delivered_from_both_transcript_shapes_busy_and_idle() {
    let body = body();
    let busy = serde_json::json!({
        "type": "attachment",
        "uuid": "b-1",
        "timestamp": iso(T0 + 5000),
        "attachment": {
            "type": "queued_command",
            "prompt": format!("<cross-session-message>\n{body}\n</cross-session-message>"),
            "origin": {"kind": "peer", "name": RELAY_NAME, "fromMode": "bypass", "body": body},
        },
    })
    .to_string();
    let idle = serde_json::json!({
        "type": "user",
        "isMeta": true,
        "uuid": "i-1",
        "timestamp": iso(T0 + 5000),
        "origin": {"kind": "peer", "from": "uds:fixture", "msg_id": "m-1"},
        "message": {
            "role": "user",
            "content": format!("Another Claude session sent a message:\n<cross-session-message from=\"uds:fixture\" from-name=\"{RELAY_NAME}\" from-mode=\"bypass\">\n{body}\n</cross-session-message>\n\nfixture hint"),
        },
    })
    .to_string();
    for line in [busy, idle] {
        let c = classify(&line);
        assert!(matches!(c, Classified::PeerMessage { .. }), "{c:?}");
        assert_eq!(
            check_delivery(&send_for(TEXT), &[c], T0 + 6000),
            SendState::Delivered
        );
    }
}

#[test]
fn delivered_when_the_relay_dropped_the_header_and_only_then() {
    let s = send_for(TEXT);
    let d = |r: Classified| check_delivery(&s, &[r], T0 + 6000);
    assert_eq!(d(peer(TEXT, T0 + 5000)), SendState::Delivered);
    assert_eq!(
        d(peer(&format!("{TEXT}\r\n "), T0 + 5000)),
        SendState::Delivered
    );
    assert_eq!(
        delivery_match(&s, &[peer(TEXT, T0 + 5000)]),
        Some("headerless")
    );
    assert_eq!(
        delivery_match(&s, &[peer(&body(), T0 + 5000)]),
        Some("full")
    );
    for b in [
        format!("{TEXT}!"),
        format!("{MESSAGE_HEADER}\n\nother"),
        MESSAGE_HEADER.to_string(),
        format!("{MESSAGE_HEADER}\n\n{MESSAGE_HEADER}\n\n{TEXT}"),
    ] {
        assert_eq!(d(peer(&b, T0 + 5000)), SendState::Pending, "{b}");
    }
    assert_eq!(
        d(peer_as(TEXT, T0 + 5000, "someone", "x")),
        SendState::Pending
    );
    assert_eq!(delivery_match(&s, &[peer(TEXT, T0 - 5000)]), None);
}

#[test]
fn delivered_when_the_body_differs_only_by_crlf_or_trailing_whitespace() {
    let crlf = format!("{}\n  ", body().replace('\n', "\r\n"));
    assert_eq!(
        check_delivery(&send_for(TEXT), &[peer(&crlf, T0 + 5000)], T0 + 6000),
        SendState::Delivered
    );
}

#[test]
fn a_record_within_1s_clock_skew_before_created_at_still_counts() {
    assert_eq!(
        check_delivery(&send_for(TEXT), &[peer(&body(), T0 - 900)], T0),
        SendState::Delivered
    );
    // The boundary is inclusive: exactly 1 s before counts, 1 ms more does not.
    assert_eq!(
        check_delivery(&send_for(TEXT), &[peer(&body(), T0 - 1000)], T0),
        SendState::Delivered
    );
    assert_eq!(
        check_delivery(&send_for(TEXT), &[peer(&body(), T0 - 1001)], T0),
        SendState::Pending
    );
}

#[test]
fn not_delivered_on_identical_text_that_predates_created_at() {
    assert_eq!(
        check_delivery(&send_for(TEXT), &[peer(&body(), T0 - 5000)], T0 + 6000),
        SendState::Pending
    );
}

#[test]
fn not_delivered_when_the_body_differs_by_one_character() {
    let b = body();
    let off = format!("{}X", &b[..b.len() - 1]);
    assert_eq!(
        check_delivery(&send_for(TEXT), &[peer(&off, T0 + 5000)], T0 + 6000),
        SendState::Pending
    );
}

#[test]
fn not_delivered_when_the_sender_is_not_the_relay() {
    assert_eq!(
        check_delivery(
            &send_for(TEXT),
            &[peer_as(&body(), T0 + 5000, "other", "o")],
            T0 + 6000
        ),
        SendState::Pending
    );
}

#[test]
fn not_delivered_on_an_unparseable_timestamp() {
    let r = Classified::PeerMessage {
        uuid: "g".into(),
        ts: "garbage".into(),
        from_name: RELAY_NAME.into(),
        from_mode: "bypass".into(),
        body: body(),
    };
    assert_eq!(
        check_delivery(&send_for(TEXT), &[r], T0 + 6000),
        SendState::Pending
    );
}

#[test]
fn held_on_a_held_notice_then_delivered_when_the_body_arrives() {
    let s = send_for(TEXT);
    let first = check_delivery(&s, &[held(T0 + 3000)], T0 + 4000);
    assert_eq!(first, SendState::Held);
    let s2 = send_with(TEXT, |s| s.state = first);
    assert_eq!(
        check_delivery(&s2, &[peer(&body(), T0 + 60_000)], T0 + 61_000),
        SendState::Delivered
    );
}

#[test]
fn held_notice_from_another_peer_or_before_created_at_is_ignored() {
    assert_eq!(
        check_delivery(&send_for(TEXT), &[held_by(T0 + 3000, "someone")], T0 + 4000),
        SendState::Pending
    );
    assert_eq!(
        check_delivery(&send_for(TEXT), &[held(T0 - 5000)], T0 + 4000),
        SendState::Pending
    );
}

#[test]
fn expired_after_the_delivery_window_pending_or_held() {
    let after = T0 + WINDOW + 1;
    assert_eq!(
        check_delivery(&send_for(TEXT), &[], after),
        SendState::Expired
    );
    assert_eq!(
        check_delivery(&send_with(TEXT, |s| s.state = SendState::Held), &[], after),
        SendState::Expired
    );
    assert_eq!(
        check_delivery(&send_for(TEXT), &[], T0 + WINDOW),
        SendState::Pending
    );
}

#[test]
fn a_late_delivery_seen_in_the_same_pass_beats_expiry() {
    assert_eq!(
        check_delivery(
            &send_for(TEXT),
            &[peer(&body(), T0 + 600_000)],
            T0 + WINDOW + 10
        ),
        SendState::Delivered
    );
}

#[test]
fn delivered_and_expired_never_change() {
    for state in [SendState::Delivered, SendState::Expired] {
        let s = send_with(TEXT, |s| s.state = state);
        assert_eq!(
            check_delivery(&s, &[peer(&body(), T0 + 5000), held(T0 + 1)], T0 + 6000),
            state
        );
        assert_eq!(check_delivery(&s, &[], T0 + WINDOW * 2), state);
    }
}

#[test]
fn failed_is_promoted_to_delivered_on_an_exact_match_inside_the_window_nothing_else() {
    let s = send_with(TEXT, |s| s.state = SendState::Failed);
    let b = body();
    assert_eq!(
        check_delivery(&s, &[peer(&b, T0 + 5000)], T0 + 6000),
        SendState::Delivered
    );
    assert_eq!(
        check_delivery(&s, &[peer(&b, T0 + 5000)], T0 + WINDOW),
        SendState::Delivered
    );
    assert_eq!(
        check_delivery(&s, &[peer(&b, T0 + 5000)], T0 + WINDOW + 1),
        SendState::Failed
    );
    assert_eq!(
        check_delivery(&s, &[peer(&format!("{b}!"), T0 + 5000)], T0 + 6000),
        SendState::Failed
    );
    assert_eq!(
        check_delivery(&s, &[peer(&b, T0 - 5000)], T0 + 6000),
        SendState::Failed
    );
    assert_eq!(
        check_delivery(&s, &[held(T0 + 1)], T0 + 6000),
        SendState::Failed
    );
    assert_eq!(check_delivery(&s, &[], T0 + WINDOW * 2), SendState::Failed);
}

// ---- parse_js_date ----

#[test]
fn parse_js_date_matches_date_parse_for_iso_forms() {
    assert_eq!(parse_js_date("2026-11-13T10:00:00.000Z"), Some(T0));
    assert_eq!(parse_js_date("2026-11-13T10:00:00Z"), Some(T0));
    assert_eq!(parse_js_date("2026-11-13T10:00Z"), Some(T0));
    assert_eq!(parse_js_date("2026-11-13T12:00:00.5+02:00"), Some(T0 + 500));
    assert_eq!(parse_js_date("2026-11-13T09:00:00-01:00"), Some(T0));
    assert_eq!(parse_js_date("2026-11-13"), Some(T0 - 10 * 3_600_000));
    assert_eq!(parse_js_date("1970-01-01T00:00:00.000Z"), Some(0));
    assert_eq!(
        parse_js_date("+010000-01-01T00:00:00.000Z"),
        Some(253_402_300_800_000)
    );
    // Like JS, any day 1..=31 is accepted and rolls over into the next month.
    assert_eq!(
        parse_js_date("2026-02-30T00:00:00Z"),
        Some(1_772_409_600_000)
    );
    assert_eq!(parse_js_date("2026-02-32T00:00:00Z"), None);
    // Fraction digits beyond 3 are truncated, fewer are padded.
    assert_eq!(parse_js_date("2026-11-13T10:00:00.1239Z"), Some(T0 + 123));
    assert_eq!(
        parse_js_date("2026-11-13T24:00:00Z"),
        Some(T0 + 14 * 3_600_000)
    );
    for bad in [
        "",
        "garbage",
        "2026-13-01T00:00:00Z",
        "2026-11-13T25:00:00Z",
        "2026-11-13T10:60:00Z",
        "2026-11-13T24:00:01Z",
        "-000000-01-01T00:00:00Z",
        "2026-11-13T10:00:00.Z",
    ] {
        assert_eq!(parse_js_date(bad), None, "{bad}");
    }
}

// ---- delivery_checker / expire_sends ----

struct Fixture {
    _dir: TempDir,
    path: std::path::PathBuf,
    store: Store,
    log: Arc<MemoryLogger>,
}

fn fixture() -> Fixture {
    let dir = tempfile::Builder::new()
        .prefix("hub-verify-")
        .tempdir()
        .unwrap();
    let path = dir.path().join("hub.db");
    let store = Store::open(&path, true).unwrap();
    Fixture {
        _dir: dir,
        path,
        store,
        log: Arc::new(MemoryLogger::new()),
    }
}

impl Fixture {
    fn checker(&self, now: i64) -> impl FnMut(&Store, &str, &[Classified]) + use<> {
        let mut check = delivery_checker(Arc::new(move || now), self.log.clone());
        move |store, id, records| check(store, id, records).expect("delivery check")
    }

    /// The row's state through a separate connection, as another process would read it.
    fn state(&self, id: i64) -> String {
        let c = Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        c.query_row("SELECT state FROM sends WHERE id = ?", [id], |r| r.get(0))
            .unwrap()
    }

    fn create(&self, text: &str, f: impl FnOnce(&mut SendRow)) -> i64 {
        self.store.create_send(&send_with(text, f)).unwrap()
    }

    fn strays(&self) -> Vec<(String, String)> {
        let s = |v: &LogValue| match v {
            LogValue::Str(s) => s.clone(),
            other => panic!("{other:?}"),
        };
        self.log
            .entries()
            .into_iter()
            .filter(|e| e.event == "relay_stray")
            .map(|e| (s(&e.fields[0].1), s(&e.fields[1].1)))
            .collect()
    }
}

#[test]
fn updates_the_store_when_the_target_sessions_transcript_shows_delivery() {
    let f = fixture();
    let id = f.create("deploy it", |_| {});
    let mut check = f.checker(T0 + 6000);
    check(
        &f.store,
        "s1",
        &[peer(&build_message("deploy it"), T0 + 5000)],
    );
    assert_eq!(f.state(id), "delivered");
    assert!(f.store.open_send_for("s1").unwrap().is_none());
}

#[test]
fn a_delivery_without_the_header_is_recorded_in_the_detail() {
    let f = fixture();
    let id = f.create("deploy it", |_| {});
    let full = f.create("ship it", |s| {
        s.id = Some(2);
        s.session_id = "s2".into();
    });
    let mut check = f.checker(T0 + 6000);
    check(&f.store, "s1", &[peer("deploy it", T0 + 5000)]);
    check(
        &f.store,
        "s2",
        &[peer(&build_message("ship it"), T0 + 5000)],
    );
    let a = f.store.get_send(id).unwrap().unwrap();
    assert_eq!(a.state, SendState::Delivered);
    assert_eq!(a.detail.as_deref(), Some(HEADERLESS_DETAIL));
    assert_eq!(
        HEADERLESS_DETAIL,
        "delivered without the Session Hub header"
    );
    let b = f.store.get_send(full).unwrap().unwrap();
    assert_eq!((b.state, b.detail), (SendState::Delivered, None));
}

#[test]
fn a_delivery_seen_in_another_sessions_transcript_never_counts() {
    let f = fixture();
    let id = f.create("deploy it", |_| {});
    f.checker(T0 + 6000)(
        &f.store,
        "s2",
        &[peer(&build_message("deploy it"), T0 + 5000)],
    );
    assert_eq!(f.state(id), "pending");
}

#[test]
fn a_relay_message_that_matches_no_send_to_that_session_is_flagged_as_stray() {
    let f = fixture();
    f.create("deploy it", |_| {}); // sent to s1
    let mut check = f.checker(T0 + 6000);
    check(
        &f.store,
        "s2",
        &[peer(&build_message("deploy it"), T0 + 5000)],
    );
    check(
        &f.store,
        "s1",
        &[peer(&build_message("rm -rf everything"), T0 + 5100)],
    );
    check(
        &f.store,
        "s1",
        &[peer_as("hello", T0 + 5200, "other-session", "o")],
    );
    assert_eq!(
        f.strays(),
        vec![
            ("s2".to_string(), iso(T0 + 5000)),
            ("s1".to_string(), iso(T0 + 5100))
        ]
    );
    assert_eq!(
        f.store.get_meta("relay_stray_count").unwrap().as_deref(),
        Some("2")
    );
    assert_eq!(
        f.store.get_meta("relay_stray_last").unwrap(),
        Some((T0 + 6000).to_string())
    );
    // Logs never carry message text.
    let dump = format!("{:?}", f.log.entries());
    assert!(!dump.contains("rm -rf") && !dump.contains("deploy it"));
}

#[test]
fn relay_messages_in_a_relay_eval_fixture_session_are_never_counted_as_stray() {
    let f = fixture();
    f.store
        .upsert_session(&SessionRow {
            session_id: "s-eval".into(),
            profile: "claude".into(),
            name: format!("{EVAL_SESSION_PREFIX}target-x1"),
            cwd: "C:/x".into(),
            pid: 1,
            kind: "background".into(),
            status: LiveStatus::Idle,
            started_at: T0,
            updated_at: T0,
            ended_at: None,
            transcript_path: "C:/x/s-eval.jsonl".into(),
            permission_mode: None,
        })
        .unwrap();
    f.checker(T0 + 6000)(
        &f.store,
        "s-eval",
        &[peer(&build_message("eval case"), T0 + 5000)],
    );
    assert!(f.strays().is_empty());
    assert_eq!(f.store.get_meta("relay_stray_count").unwrap(), None);
    assert_eq!(f.store.get_meta("relay_stray_last").unwrap(), None);
}

#[test]
fn a_relay_record_older_than_retention_is_never_flagged() {
    let f = fixture();
    let now = T0 + 10 * 24 * 3_600_000;
    f.checker(now)(
        &f.store,
        "s1",
        &[peer(&build_message("an old send whose row was purged"), T0)],
    );
    assert!(f.strays().is_empty());
    assert_eq!(f.store.get_meta("relay_stray_count").unwrap(), None);
}

#[test]
fn a_relay_record_with_an_unparseable_timestamp_is_never_flagged() {
    let f = fixture();
    let r = Classified::PeerMessage {
        uuid: "g".into(),
        ts: "garbage".into(),
        from_name: RELAY_NAME.into(),
        from_mode: "bypass".into(),
        body: "x".into(),
    };
    f.checker(T0)(&f.store, "s1", &[r]);
    assert!(f.strays().is_empty());
}

#[test]
fn a_headerless_relay_body_of_a_known_send_is_not_stray() {
    let f = fixture();
    f.create("deploy it", |s| s.state = SendState::Delivered);
    f.checker(T0 + 6000)(&f.store, "s1", &[peer("deploy it", T0 + 5000)]);
    assert!(f.strays().is_empty());
    assert_eq!(f.store.get_meta("relay_stray_count").unwrap(), None);
}

#[test]
fn a_relay_message_matching_a_send_to_that_session_in_any_state_is_not_stray() {
    let f = fixture();
    f.create("deploy it", |s| s.state = SendState::Delivered);
    f.checker(T0 + 6000)(
        &f.store,
        "s1",
        &[peer(&build_message("deploy it"), T0 + 5000)],
    );
    assert!(f.strays().is_empty());
    assert_eq!(f.store.get_meta("relay_stray_count").unwrap(), None);
}

#[test]
fn held_then_delivered_across_passes() {
    let f = fixture();
    let id = f.create("ok", |_| {});
    let mut check = f.checker(T0 + 10_000);
    check(&f.store, "s1", &[held(T0 + 2000)]);
    assert_eq!(f.state(id), "held");
    check(&f.store, "s1", &[]);
    assert_eq!(f.state(id), "held");
    check(&f.store, "s1", &[peer(&build_message("ok"), T0 + 9000)]);
    assert_eq!(f.state(id), "delivered");
}

#[test]
fn two_overridden_identical_sends_one_record_delivers_only_the_older_one() {
    let f = fixture();
    let a = f.create("yes", |s| s.created_at = T0);
    let b = f.create("yes", |s| s.created_at = T0 + 1000);
    let mut check = f.checker(T0 + 20_000);
    let m = build_message("yes");
    check(&f.store, "s1", &[peer_as(&m, T0 + 5000, RELAY_NAME, "r1")]);
    assert_eq!(f.state(a), "delivered");
    assert_eq!(f.state(b), "pending");
    check(
        &f.store,
        "s1",
        &[peer_as(&m, T0 + 15_000, RELAY_NAME, "r2")],
    );
    assert_eq!(f.state(b), "delivered");
}

#[test]
fn expire_sends_marks_old_pending_and_held_sends_expired_and_counts_them() {
    let f = fixture();
    let old1 = f.create("a", |s| s.created_at = T0);
    let old2 = f.create("b", |s| {
        s.session_id = "s2".into();
        s.created_at = T0;
        s.state = SendState::Held;
    });
    let fresh = f.create("c", |s| {
        s.session_id = "s3".into();
        s.created_at = T0 + 60_000;
    });
    let failed = f.create("d", |s| {
        s.session_id = "s4".into();
        s.created_at = T0;
        s.state = SendState::Failed;
    });
    let now = T0 + WINDOW + 1;
    assert_eq!(expire_sends(&f.store, now).unwrap(), 2);
    assert_eq!(f.state(old1), "expired");
    assert_eq!(f.state(old2), "expired");
    assert_eq!(f.state(fresh), "pending");
    assert_eq!(f.state(failed), "failed");
    assert_eq!(expire_sends(&f.store, now).unwrap(), 0);
}

#[test]
fn delivery_checker_also_expires_the_sessions_open_send_after_the_window() {
    let f = fixture();
    let id = f.create("x", |_| {});
    f.checker(T0 + WINDOW + 1)(&f.store, "s1", &[]);
    assert_eq!(f.state(id), "expired");
}

#[test]
fn a_failed_send_is_promoted_to_delivered_inside_the_window_not_after_it() {
    let f = fixture();
    let early = f.create("go", |s| {
        s.state = SendState::Failed;
        s.detail = Some("relay timed out".into());
    });
    f.checker(T0 + 60_000)(&f.store, "s1", &[peer(&build_message("go"), T0 + 50_000)]);
    assert_eq!(f.state(early), "delivered");
    assert_eq!(f.store.get_send(early).unwrap().unwrap().detail, None);

    let late = f.create("go", |s| {
        s.session_id = "s2".into();
        s.state = SendState::Failed;
    });
    f.checker(T0 + WINDOW + 1)(&f.store, "s2", &[peer(&build_message("go"), T0 + 50_000)]);
    assert_eq!(f.state(late), "failed");
}

#[test]
fn a_held_notice_applies_only_to_the_newest_pending_send_of_the_session() {
    let f = fixture();
    let older = f.create("first", |s| s.created_at = T0);
    let newer = f.create("second", |s| s.created_at = T0 + 1000);
    f.checker(T0 + 5000)(&f.store, "s1", &[held(T0 + 3000)]);
    assert_eq!(f.state(older), "pending");
    assert_eq!(f.state(newer), "held");
}

#[test]
fn expire_sends_returns_a_store_error_instead_of_swallowing_it() {
    let f = fixture();
    f.create("a", |s| s.created_at = T0);
    let ro = Store::open_read_only(&f.path).unwrap();
    // The read of open sends works; the expiring write fails on the read-only connection.
    assert!(expire_sends(&ro, T0 + WINDOW + 1).is_err());
    assert_eq!(f.state(1), "pending");
}

// ---- the relay changed the line break after the header (#16) ----

#[test]
fn delivered_when_the_relay_changed_only_the_line_break_after_the_header() {
    // Seen live: the relay model sent the header and the text with one line break between them.
    for sep in ["\n", "\r\n", "\n\n\n", " \n", "\n \n"] {
        let b = format!("{MESSAGE_HEADER}{sep}{TEXT}");
        assert_eq!(
            check_delivery(&send_for(TEXT), &[peer(&b, T0 + 5000)], T0 + 6000),
            SendState::Delivered,
            "{sep:?}"
        );
        assert_eq!(
            delivery_match(&send_for(TEXT), &[peer(&b, T0 + 5000)]),
            Some("reflowed"),
            "{sep:?}"
        );
    }
    // The owner's text itself must still match exactly; no line break at all is not a separator.
    for b in [
        format!("{MESSAGE_HEADER}\n{TEXT}!"),
        format!("{MESSAGE_HEADER}\n {TEXT}"),
        format!("{MESSAGE_HEADER}{TEXT}"),
        format!("{MESSAGE_HEADER} {TEXT}"),
    ] {
        assert_eq!(
            check_delivery(&send_for(TEXT), &[peer(&b, T0 + 5000)], T0 + 6000),
            SendState::Pending,
            "{b:?}"
        );
    }
}

#[test]
fn a_reflowed_delivery_notes_it_and_the_other_rules_still_hold() {
    assert_eq!(
        REFLOWED_DETAIL,
        "delivered; the relay changed the line break after the header"
    );
    let f = fixture();
    let id = f.create("ok", |_| {});
    f.checker(T0 + 6000)(
        &f.store,
        "s1",
        &[peer(&format!("{MESSAGE_HEADER}\nok"), T0 + 5000)],
    );
    let a = f.store.get_send(id).unwrap().unwrap();
    assert_eq!(a.state, SendState::Delivered);
    assert_eq!(a.detail.as_deref(), Some(REFLOWED_DETAIL));
    // Same rules as a full match: not from the relay, or before the send, proves nothing.
    let b = format!("{MESSAGE_HEADER}\nok");
    assert_eq!(
        check_delivery(
            &send_for("ok"),
            &[peer_as(&b, T0 + 5000, "someone", "x")],
            T0 + 6000
        ),
        SendState::Pending
    );
    assert_eq!(
        delivery_match(&send_for("ok"), &[peer(&b, T0 - 5000)]),
        None
    );
}

#[test]
fn a_known_send_whose_header_line_break_the_relay_changed_is_not_stray() {
    let f = fixture();
    f.create("Recommended all are approved", |_| {});
    f.checker(T0 + 6000)(
        &f.store,
        "s1",
        &[peer(
            &format!("{MESSAGE_HEADER}\nRecommended all are approved"),
            T0 + 5000,
        )],
    );
    assert!(f.strays().is_empty());
    assert_eq!(f.store.get_meta("relay_stray_count").unwrap(), None);
}
