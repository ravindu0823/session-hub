//! Port of `src/tui/format.ts` and of `test/tui/status-lines.test.ts`.
use hub_core::status::WaitingItem;
use hub_core::store::{LatestReply, LiveStatus, Role, SessionView, StoredStatus, ViewWaitingItem};
use hub_core::view::snapshot::{DaemonState, SnapshotMeta, StaleProfile};
use hub_tui::format::{
    ago, hhmm, parse_ts, profile_header, role_label, session_label, state_label, wrap,
};
use hub_tui::status_pane::status_lines;
use ratatui::style::Color;

#[test]
fn ago_is_short_and_never_negative() {
    assert_eq!(ago(0), "0s");
    assert_eq!(ago(45_000), "45s");
    assert_eq!(ago(59_999), "59s");
    assert_eq!(ago(60_000), "1m");
    assert_eq!(ago(3_599_000), "59m");
    assert_eq!(ago(3_600_000), "1h");
    assert_eq!(ago(47 * 3_600_000), "47h");
    assert_eq!(ago(48 * 3_600_000), "2d");
    // Clock skew reads as 0s.
    assert_eq!(ago(-5_000), "0s");
}

#[test]
fn hhmm_formats_local_time_and_rejects_garbage() {
    assert_eq!(hhmm("not a date"), "??:??");
    assert_eq!(hhmm(""), "??:??");
    assert_eq!(hhmm("2026-13-40T10:00:00Z"), "??:??");
    // No offset: local wall time, shown as written.
    assert_eq!(hhmm("2026-11-14T10:05:00"), "10:05");
    // The same instant in two spellings shows the same local time.
    let a = hhmm("2026-11-14T10:05:00.000+05:30");
    assert_eq!(a, hhmm("2026-11-14T04:35:00.000Z"));
    let b = a.as_bytes();
    assert!(b.len() == 5 && b[2] == b':' && b[..2].iter().chain(&b[3..]).all(u8::is_ascii_digit));
}

#[test]
fn parse_ts_is_date_parse_for_iso_forms() {
    assert_eq!(parse_ts("1970-01-01T00:00:00.000Z"), Some(0));
    assert_eq!(parse_ts("1970-01-01T00:00:01Z"), Some(1000));
    assert_eq!(
        parse_ts("2026-11-14T10:00:00.000Z"),
        Some(1_794_650_400_000)
    );
    assert_eq!(parse_ts("1970-01-02"), Some(86_400_000));
    assert_eq!(parse_ts("1970-01-01T01:00:00+01:00"), Some(0));
    assert_eq!(parse_ts("1970-01-01T00:00:00.5Z"), Some(500));
    assert_eq!(parse_ts("nope"), None);
    assert_eq!(parse_ts("2026-02-30T00:00:00Z"), None);
}

#[test]
fn session_label_is_the_name_or_the_first_8_chars_of_the_id() {
    assert_eq!(session_label("alpha", "s1"), "alpha");
    assert_eq!(session_label("", "abcdefghijk"), "abcdefgh");
    // Both are cleaned: an escape sequence never reaches the terminal.
    assert_eq!(session_label("\x1b[31mX", "i"), "X");
    assert_eq!(session_label("", "\x1b]0;title\x07abcdefghij"), "abcdefgh");
    // 8 UTF-16 units: an emoji counts 2.
    assert_eq!(
        session_label("", "ab\u{1F600}\u{1F600}\u{1F600}\u{1F600}x"),
        "ab\u{1F600}\u{1F600}\u{1F600}"
    );
}

#[test]
fn state_label_reads_ended_before_state() {
    assert_eq!(state_label(LiveStatus::Busy, None), "busy");
    assert_eq!(state_label(LiveStatus::Idle, None), "idle");
    assert_eq!(state_label(LiveStatus::Waiting, None), "needs you");
    assert_eq!(state_label(LiveStatus::Unknown, None), "unknown");
    assert_eq!(state_label(LiveStatus::Waiting, Some(5)), "ended");
}

#[test]
fn profile_header_and_role_label() {
    assert_eq!(profile_header("claude"), ".claude");
    assert_eq!(profile_header("claude-personal"), ".claude-personal");
    assert_eq!(role_label(Role::Owner, None), "you");
    assert_eq!(role_label(Role::Peer, Some("bob")), "peer:bob");
    assert_eq!(role_label(Role::Peer, None), "peer:?");
    assert_eq!(role_label(Role::Assistant, None), "claude");
}

#[test]
fn wrap_breaks_on_spaces_then_hard() {
    assert_eq!(wrap("hello world foo", 11, ""), ["hello world", "foo"]);
    assert_eq!(wrap("abcdefghij", 4, ""), ["abcd", "efgh", "ij"]);
    assert_eq!(wrap("a\n\nb", 10, ""), ["a", "", "b"]);
    assert_eq!(wrap("", 10, ""), [""]);
    // Leading spaces of a paragraph are dropped (the TS loop starts on an empty line).
    assert_eq!(wrap("  Q1 open", 20, ""), ["Q1 open"]);
    // Width is at least 4.
    assert_eq!(wrap("abcdef", 1, ""), ["abcd", "ef"]);
    // By code point, not by byte.
    assert_eq!(wrap("日本語日本語", 4, ""), ["日本語日", "本語"]);
}

#[test]
fn wrap_indents_continuation_lines() {
    assert_eq!(
        wrap("  1. aaa bbb ccc", 10, "     "),
        ["1. aaa bbb", "     ccc"]
    );
}

#[test]
fn wrap_with_an_indent_wider_than_the_width_still_ends() {
    let out = wrap("aaaa  bbbb", 4, "       ");
    assert!(!out.is_empty() && out.len() < 20);
}

// ---- test/tui/status-lines.test.ts ----

const NOW: i64 = 1_794_650_400_000 + 600_000; // 2026-11-14T10:10:00Z
fn meta() -> SnapshotMeta {
    SnapshotMeta {
        unknown_count: 0,
        unknown_version: None,
        stale: vec![],
        daemon: DaemonState::Running,
        relay_stray_count: 0,
        unreadable_sessions: 0,
    }
}

fn item(id: &str, text: &str, is_new: bool, children: &[&str], answered: bool) -> ViewWaitingItem {
    ViewWaitingItem {
        item: WaitingItem {
            id: Some(id.into()),
            text: text.into(),
            is_new,
            children: children.iter().map(|c| c.to_string()).collect(),
        },
        answered,
    }
}

fn view() -> SessionView {
    SessionView {
        session_id: "s1".into(),
        profile: "claude".into(),
        name: "alpha".into(),
        cwd: "C:/x".into(),
        pid: 1,
        kind: "interactive".into(),
        started_at: 0,
        updated_at: 0,
        ended_at: None,
        transcript_path: "C:/x/s1.jsonl".into(),
        permission_mode: None,
        state: LiveStatus::Idle,
        latest_reply: None,
        waiting_count: 1,
        status: Some(StoredStatus {
            progress_pct: None,
            phase: None,
            remaining: vec![],
            waiting: vec![
                item("Q1", "open one", false, &[], false),
                item("Q2", "done one", true, &["detail"], true),
            ],
            recommendations: vec![],
            reply_uuid: "r1".into(),
            reply_ts: "2026-11-14T10:00:00.000Z".into(),
        }),
    }
}

#[test]
fn an_answered_item_is_left_out_an_open_one_is_shown() {
    let lines = status_lines(&view(), &meta(), NOW, 120);
    let open = lines.iter().find(|l| l.text.contains("open one")).unwrap();
    assert_eq!(open.text, "Q1 open one");
    assert!(!open.dim);
    assert!(
        !lines
            .iter()
            .any(|l| l.text.contains("done one") || l.text.contains("detail"))
    );
}

#[test]
fn when_every_item_is_answered_the_list_says_nothing() {
    let mut v = view();
    for w in &mut v.status.as_mut().unwrap().waiting {
        w.answered = true;
    }
    let lines = status_lines(&v, &meta(), NOW, 120);
    let at = lines
        .iter()
        .position(|l| l.text == "Waiting on you")
        .unwrap();
    assert_eq!(lines[at + 1].text, "nothing");
    assert!(lines[at + 1].dim);
}

#[test]
fn relay_messages_that_reached_the_wrong_session_are_flagged() {
    let wrong = |m: &SnapshotMeta| {
        status_lines(&view(), m, NOW, 120)
            .into_iter()
            .find(|l| l.text.contains("wrong session"))
    };
    assert!(wrong(&meta()).is_none());
    let warn = wrong(&SnapshotMeta {
        relay_stray_count: 2,
        ..meta()
    })
    .unwrap();
    assert_eq!(
        warn.text,
        "relay: 2 messages reached the wrong session (see run\\relay.log)"
    );
    assert_eq!(warn.color, Some(Color::Red));
}

#[test]
fn status_lines_full_layout() {
    let mut v = view();
    let st = v.status.as_mut().unwrap();
    st.progress_pct = Some(40);
    st.phase = Some("build".into());
    st.remaining = vec!["one".into(), "two".into()];
    st.recommendations = vec!["rec".into()];
    v.latest_reply = Some(LatestReply {
        reply_uuid: "r2".into(),
        reply_ts: "2026-11-14T10:05:00.000Z".into(),
        has_status: false,
    });
    let m = SnapshotMeta {
        unknown_count: 3,
        unknown_version: Some("9".into()),
        stale: vec![StaleProfile {
            profile: "claude".into(),
            since: NOW - 120_000,
        }],
        ..meta()
    };
    let texts: Vec<String> = status_lines(&v, &m, NOW, 120)
        .into_iter()
        .map(|l| l.text)
        .collect();
    let newer = format!(
        "newer reply at {} has no status block",
        hhmm("2026-11-14T10:05:00.000Z")
    );
    let from = format!(
        "status from 10m ago (reply at {} local)",
        hhmm("2026-11-14T10:00:00.000Z")
    );
    assert_eq!(
        texts,
        [
            "stale: .claude list not updated for 2m",
            "format changed: 3 unknown records skipped (version 9)",
            newer.as_str(),
            from.as_str(),
            "Progress: 40% overall. Phase: build",
            "",
            "What's remaining",
            "1. one",
            "2. two",
            "",
            "Waiting on you",
            "Q1 open one",
            "",
            "Recommendations",
            "* rec",
        ]
    );
}

#[test]
fn no_status_reply_yet() {
    let mut v = view();
    v.status = None;
    let t: Vec<String> = status_lines(&v, &meta(), NOW, 80)
        .into_iter()
        .map(|l| l.text)
        .collect();
    assert_eq!(t, ["no status reply yet"]);
}

/// Run with `TZ=Asia/Kolkata` (UTC+5:30, no DST) or `TZ=America/New_York`; skipped otherwise.
#[cfg(unix)]
#[test]
fn hhmm_follows_the_process_time_zone_when_tz_is_set() {
    match std::env::var("TZ").as_deref() {
        Ok("Asia/Kolkata") => assert_eq!(hhmm("2026-11-14T04:35:00.000Z"), "10:05"),
        Ok("America/New_York") => {
            // EST in November, EDT in July: the offset is the one in force at that instant.
            assert_eq!(hhmm("2026-11-14T15:00:00Z"), "10:00");
            assert_eq!(hhmm("2026-07-14T15:00:00Z"), "11:00");
        }
        _ => {}
    }
}

#[test]
fn unreadable_session_rows_are_flagged_with_their_count_only() {
    let line = |m: &SnapshotMeta| {
        status_lines(&view(), m, NOW, 120)
            .into_iter()
            .find(|l| l.text.contains("unreadable"))
    };
    assert!(line(&meta()).is_none());
    let one = line(&SnapshotMeta {
        unreadable_sessions: 1,
        ..meta()
    })
    .unwrap();
    assert_eq!(one.text, "index: 1 session row unreadable, not listed");
    assert_eq!(one.color, Some(Color::Red));
    let two = line(&SnapshotMeta {
        unreadable_sessions: 2,
        ..meta()
    })
    .unwrap();
    assert_eq!(two.text, "index: 2 session rows unreadable, not listed");
}
