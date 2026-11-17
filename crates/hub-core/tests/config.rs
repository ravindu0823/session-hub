use hub_core::config::{Os, ProfileId, resolve_config};
use hub_core::text::{clip_utf16, utf16_len};
use std::collections::HashMap;

fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let m: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k| m.get(k).cloned()
}

fn s(p: &std::path::Path) -> String {
    p.to_str().unwrap().to_string()
}

#[test]
fn windows_paths() {
    let e = env(&[
        ("USERPROFILE", r"C:\Users\a"),
        ("LOCALAPPDATA", r"C:\Users\a\AppData\Local"),
    ]);
    let c = resolve_config(&e, Os::Windows).unwrap();
    assert_eq!(s(&c.profiles[0].dir), r"C:\Users\a\.claude");
    assert_eq!(s(&c.profiles[1].dir), r"C:\Users\a\.claude-personal");
    assert_eq!(s(&c.data_dir), r"C:\Users\a\AppData\Local\SessionHub");
    assert_eq!(s(&c.run_dir()), r"C:\Users\a\AppData\Local\SessionHub\run");
    assert_eq!(
        s(&c.relay_dir()),
        r"C:\Users\a\AppData\Local\SessionHub\relay"
    );
    assert_eq!(
        s(&c.db_path()),
        r"C:\Users\a\AppData\Local\SessionHub\hub.db"
    );
}

#[test]
fn windows_requires_env() {
    let e = env(&[("LOCALAPPDATA", "x")]);
    let err = resolve_config(&e, Os::Windows).unwrap_err();
    assert!(err.to_string().contains("USERPROFILE is not set"));
    let e = env(&[("USERPROFILE", "x")]);
    let err = resolve_config(&e, Os::Windows).unwrap_err();
    assert!(err.to_string().contains("LOCALAPPDATA is not set"));
}

#[test]
fn macos_paths() {
    let c = resolve_config(&env(&[("HOME", "/Users/a")]), Os::MacOs).unwrap();
    assert_eq!(s(&c.profiles[0].dir), "/Users/a/.claude");
    assert_eq!(s(&c.profiles[1].dir), "/Users/a/.claude-personal");
    assert_eq!(
        s(&c.data_dir),
        "/Users/a/Library/Application Support/SessionHub"
    );
    assert_eq!(
        s(&c.db_path()),
        "/Users/a/Library/Application Support/SessionHub/hub.db"
    );
}

#[test]
fn unix_requires_home() {
    let err = resolve_config(&env(&[]), Os::Linux).unwrap_err();
    assert!(err.to_string().contains("HOME is not set"));
}

#[test]
fn linux_paths() {
    let c = resolve_config(&env(&[("HOME", "/home/a")]), Os::Linux).unwrap();
    assert_eq!(s(&c.data_dir), "/home/a/.local/share/session-hub");
    let c = resolve_config(
        &env(&[("HOME", "/home/a"), ("XDG_DATA_HOME", "/x")]),
        Os::Linux,
    )
    .unwrap();
    assert_eq!(s(&c.data_dir), "/x/session-hub");
    let c = resolve_config(
        &env(&[("HOME", "/home/a"), ("XDG_DATA_HOME", "rel")]),
        Os::Linux,
    )
    .unwrap();
    assert_eq!(s(&c.data_dir), "/home/a/.local/share/session-hub");
}

#[test]
fn only_two_profiles() {
    let c = resolve_config(&env(&[("HOME", "/h")]), Os::Linux).unwrap();
    assert_eq!(c.profiles.len(), 2);
    assert_eq!(c.profiles[0].id, ProfileId::Claude);
    assert_eq!(c.profiles[1].id, ProfileId::ClaudePersonal);
    assert_eq!(c.profiles[0].id.as_str(), "claude");
    assert_eq!(c.profiles[1].id.as_str(), "claude-personal");
}

#[test]
fn constants_match_ts() {
    use hub_core::config::*;
    assert_eq!(RETENTION_MS, 604_800_000);
    assert_eq!(FIRST_INDEX_BYTES, 20_971_520);
    assert_eq!(DELIVERY_WINDOW_MS, 1_800_000);
    assert_eq!(STRAY_WARN_MS, 86_400_000);
    assert_eq!(DEBOUNCE_MS, 500);
    assert_eq!(SAFETY_POLL_MS, 30_000);
    assert_eq!(AGENTS_TIMEOUT_MS, 10_000);
    assert_eq!(EXPIRE_EVERY_MS, 30_000);
    assert_eq!(RELAY_NAME, "session-hub-relay");
    assert_eq!(EVAL_SESSION_PREFIX, "sh-eval-");
    assert_eq!(RELAY_LOG, "relay.log");
    assert_eq!(DAEMON_STOPPED_FILE, "daemon.stopped");
    assert!(MESSAGE_HEADER.starts_with("[Session Hub: message from the owner."));
    assert!(MESSAGE_HEADER.ends_with("the sender has exited.]"));
    let _ = current_os();
}

#[test]
fn clip_utf16_boundaries() {
    assert_eq!(clip_utf16("ab😀c", 3), "ab");
    assert_eq!(clip_utf16("ab😀c", 4), "ab😀");
    assert_eq!(clip_utf16("ab", 10), "ab");
    assert_eq!(utf16_len("😀"), 2);
}
