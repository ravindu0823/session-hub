//! Port of `test/discovery/agents.test.ts` and `watch.test.ts` (the relay half of the TS fake is
//! not needed yet), with the fake claude as a bin target.
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hub_core::config::{DEBOUNCE_MS, RELAY_NAME};
use hub_core::discovery::{
    AgentsError, ClaudeCli, LiveSession, RealClaudeCli, StopToken, find_claude, is_own_relay,
    parse_agents, parse_status, should_fire, watch_sessions, watch_sessions_with,
};
use hub_core::platform::{FakePlatform, FocusResult, Platform};
use hub_core::store::LiveStatus;
use serde_json::{Value, json};
use tempfile::TempDir;

fn tmp(prefix: &str) -> TempDir {
    tempfile::Builder::new().prefix(prefix).tempdir().unwrap()
}

// ---------------------------------------------------------------- parse

fn entry() -> Value {
    json!({
        "pid": 1, "cwd": "D:\\x", "kind": "interactive", "startedAt": 5,
        "sessionId": "s1", "name": "n", "status": "idle"
    })
}

#[test]
fn parse_agents_parses_real_shape_entries() {
    let r = parse_agents(&json!([entry()]));
    assert_eq!(
        r,
        vec![LiveSession {
            pid: 1,
            cwd: "D:\\x".into(),
            kind: "interactive".into(),
            started_at: 5,
            session_id: "s1".into(),
            name: "n".into(),
            status: LiveStatus::Idle,
        }]
    );
    // camelCase on the wire, like the TS object.
    assert_eq!(serde_json::to_value(&r[0]).unwrap(), entry());
}

#[test]
fn parse_agents_missing_fields_get_defaults_and_bad_status_is_unknown() {
    let r = parse_agents(&json!([
        { "pid": 2, "sessionId": "s2" },
        { "pid": 3, "sessionId": "s3", "status": "weird" },
        { "pid": 4, "sessionId": "s4", "status": 7 },
    ]));
    assert_eq!(
        r[0],
        LiveSession {
            pid: 2,
            cwd: "".into(),
            kind: "unknown".into(),
            started_at: 0,
            session_id: "s2".into(),
            name: "".into(),
            status: LiveStatus::Unknown,
        }
    );
    assert_eq!(r[1].status, LiveStatus::Unknown);
    assert_eq!(r[2].status, LiveStatus::Unknown);
}

#[test]
fn parse_agents_skips_entries_without_session_id_or_numeric_pid_and_non_arrays() {
    assert!(
        parse_agents(&json!([
            { "pid": 1 }, { "sessionId": "a" }, { "pid": "1", "sessionId": "a" },
            { "pid": 1, "sessionId": 5 }, null, 3, "x", []
        ]))
        .is_empty()
    );
    assert!(parse_agents(&json!({})).is_empty());
    assert!(parse_agents(&json!(null)).is_empty());
}

#[test]
fn parse_status_accepts_only_the_three_names() {
    assert_eq!(parse_status(&json!("busy")), LiveStatus::Busy);
    assert_eq!(parse_status(&json!("idle")), LiveStatus::Idle);
    assert_eq!(parse_status(&json!("waiting")), LiveStatus::Waiting);
    assert_eq!(parse_status(&json!("unknown")), LiveStatus::Unknown);
    assert_eq!(parse_status(&json!("BUSY")), LiveStatus::Unknown);
    assert_eq!(parse_status(&json!(null)), LiveStatus::Unknown);
}

/// R7: like JS `typeof === "number"`, any JSON number is a pid or a start time, truncated.
#[test]
fn parse_agents_takes_any_number_for_pid_and_started_at_truncated_to_i64() {
    let r = parse_agents(&json!([
        { "pid": 12.7, "startedAt": 99.9, "sessionId": "a" },
        { "pid": -3.5, "startedAt": 1e3, "sessionId": "b" },
        { "pid": 5.0, "sessionId": "c" },
        { "pid": 1e300, "sessionId": "skipped-pid-out-of-range" },
        { "pid": -1e300, "sessionId": "skipped-pid-too-small" },
        { "pid": 18446744073709551615u64, "sessionId": "skipped-u64" },
        { "pid": 7, "startedAt": 1e300, "sessionId": "d" },
        { "pid": 9, "startedAt": "5", "sessionId": "e" },
    ]));
    let got: Vec<(&str, i64, i64)> = r
        .iter()
        .map(|s| (s.session_id.as_str(), s.pid, s.started_at))
        .collect();
    assert_eq!(
        got,
        [
            ("a", 12, 99),
            ("b", -3, 1000),
            ("c", 5, 0),
            ("d", 7, 0),
            ("e", 9, 0)
        ]
    );
}

// ---------------------------------------------------------------- is_own_relay

fn relay_base(name: &str, cwd: &str) -> LiveSession {
    LiveSession {
        pid: 1,
        cwd: cwd.into(),
        kind: "x".into(),
        started_at: 0,
        session_id: "s".into(),
        name: name.into(),
        status: LiveStatus::Idle,
    }
}

#[test]
fn is_own_relay_matches_name_and_cwd() {
    let t = tmp("hub-agents-");
    let dir = t.path().join("relay");
    let d = dir.to_string_lossy().to_string();
    assert!(is_own_relay(&relay_base(RELAY_NAME, &d), &dir));
    let sep = std::path::MAIN_SEPARATOR;
    assert!(is_own_relay(
        &relay_base(RELAY_NAME, &format!("{d}{sep}")),
        &dir
    ));
    // `..` and `.` are resolved like path.resolve does.
    assert!(is_own_relay(
        &relay_base(RELAY_NAME, &format!("{d}{sep}x{sep}..{sep}.")),
        &dir
    ));
}

#[test]
fn is_own_relay_other_name_or_cwd_does_not_match() {
    let t = tmp("hub-agents-");
    let dir = t.path().join("relay");
    let d = dir.to_string_lossy().to_string();
    assert!(!is_own_relay(&relay_base("other", &d), &dir));
    let elsewhere = t.path().join("elsewhere").to_string_lossy().to_string();
    assert!(!is_own_relay(&relay_base(RELAY_NAME, &elsewhere), &dir));
    assert!(!is_own_relay(&relay_base(RELAY_NAME, ""), &dir));
    assert!(!is_own_relay(&relay_base(RELAY_NAME, "relay"), &dir));
}

#[test]
fn is_own_relay_is_case_sensitive_off_windows() {
    if cfg!(windows) {
        return;
    }
    let dir = PathBuf::from("/tmp/Relay");
    assert!(!is_own_relay(&relay_base(RELAY_NAME, "/tmp/relay"), &dir));
}

#[cfg(windows)]
#[test]
fn is_own_relay_case_insensitive_on_windows() {
    let t = tmp("hub-agents-");
    let dir = t.path().join("relay");
    let up = dir.to_string_lossy().to_uppercase();
    assert!(is_own_relay(&relay_base(RELAY_NAME, &up), &dir));
}

// ---------------------------------------------------------------- find_claude

#[test]
fn find_claude_searches_path_for_the_platform_name() {
    let a = tmp("hub-path-a-");
    let b = tmp("hub-path-b-");
    let name = if cfg!(windows) {
        "claude.cmd"
    } else {
        "claude"
    };
    let exe = b.path().join(name);
    std::fs::write(&exe, "x").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = std::env::join_paths([a.path(), b.path()]).unwrap();
    assert_eq!(find_claude(&path), Some(exe.clone()));
    // Not on this PATH: nothing.
    let only_a = std::env::join_paths([a.path()]).unwrap();
    assert_eq!(find_claude(&only_a), None);
    assert_eq!(find_claude(&OsString::new()), None);
}

#[cfg(windows)]
#[test]
fn find_claude_prefers_exe_over_cmd_on_windows() {
    let a = tmp("hub-path-");
    std::fs::write(a.path().join("claude.cmd"), "x").unwrap();
    std::fs::write(a.path().join("claude.exe"), "x").unwrap();
    let path = std::env::join_paths([a.path()]).unwrap();
    assert_eq!(find_claude(&path), Some(a.path().join("claude.exe")));
}

#[cfg(unix)]
#[test]
fn find_claude_skips_a_file_that_is_not_executable_and_a_directory() {
    use std::os::unix::fs::PermissionsExt;
    let a = tmp("hub-path-a-");
    let b = tmp("hub-path-b-");
    std::fs::write(a.path().join("claude"), "x").unwrap();
    std::fs::set_permissions(
        a.path().join("claude"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    std::fs::create_dir(b.path().join("claude")).unwrap();
    let path = std::env::join_paths([a.path(), b.path()]).unwrap();
    assert_eq!(find_claude(&path), None);
}

// ---------------------------------------------------------------- StopToken / FakePlatform

#[test]
fn stop_token_wait_timeout_returns_false_until_stopped_then_true() {
    let s = StopToken::new();
    assert!(!s.is_stopped());
    let t0 = Instant::now();
    assert!(!s.wait_timeout(Duration::from_millis(50)));
    assert!(t0.elapsed() >= Duration::from_millis(40));
    let s2 = s.clone();
    let h = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        s2.stop();
    });
    let t0 = Instant::now();
    assert!(s.wait_timeout(Duration::from_secs(10)));
    assert!(t0.elapsed() < Duration::from_secs(5));
    assert!(s.is_stopped());
    // Already stopped: returns at once.
    assert!(s.wait_timeout(Duration::from_secs(10)));
    h.join().unwrap();
}

#[test]
fn fake_platform_rest_of_the_interface() {
    let t = tmp("hub-fakeplat-");
    let fp = FakePlatform;
    fp.ensure_private_dir(t.path()).unwrap();
    assert!(fp.is_private_dir(t.path()).unwrap());
    assert!(fp.process_alive(std::process::id()));
    assert!(!fp.process_alive(0));
    assert_eq!(fp.process_start_time(std::process::id()), None);
    assert_eq!(fp.process_image_path(std::process::id()), None);
    assert_eq!(
        fp.focus_session_window(std::process::id()),
        FocusResult::Unsupported
    );
}

#[test]
fn fake_platform_kill_tree_really_kills_and_reports_a_missing_pid() {
    let exe = env!("CARGO_BIN_EXE_fake-claude");
    let mut child = Command::new(exe)
        .env("FAKE_CLAUDE_MODE", "sleep-forever")
        .spawn()
        .unwrap();
    let fp = FakePlatform;
    fp.spawn_hidden(&mut Command::new(exe));
    fp.kill_tree(child.id()).unwrap();
    assert!(!child.wait().unwrap().success());
    // The pid is gone (and reaped): killing again fails.
    assert!(fp.kill_tree(child.id()).is_err());
}

/// Ruling R11: pids whose kill would reach other processes are refused without running anything.
/// pid 0 is checked only after pid 1: an unguarded `kill -9 0` would end this test run's group.
#[test]
fn fake_platform_kill_tree_refuses_dangerous_pids() {
    let fp = FakePlatform;
    let refused = |pid: u32| {
        fp.kill_tree(pid)
            .is_err_and(|e| e.kind() == std::io::ErrorKind::InvalidInput)
    };
    assert!(refused(if cfg!(windows) { 4 } else { 1 }));
    assert!(refused(0));
    assert!(refused(std::process::id()));
}

// ---------------------------------------------------------------- realClaudeCli with fake

/// Passes env to the fake child through the spawn hook, so no test touches the process env.
struct EnvPlatform(HashMap<String, String>);

/// Everything but spawn and kill goes to [`FakePlatform`].
macro_rules! delegate_rest_to_fake {
    () => {
        fn ensure_private_dir(&self, dir: &Path) -> std::io::Result<()> {
            FakePlatform.ensure_private_dir(dir)
        }
        fn is_private_dir(&self, dir: &Path) -> std::io::Result<bool> {
            FakePlatform.is_private_dir(dir)
        }
        fn process_alive(&self, pid: u32) -> bool {
            FakePlatform.process_alive(pid)
        }
        fn process_start_time(&self, pid: u32) -> Option<i64> {
            FakePlatform.process_start_time(pid)
        }
        fn process_image_path(&self, pid: u32) -> Option<PathBuf> {
            FakePlatform.process_image_path(pid)
        }
        fn focus_session_window(&self, pid: u32) -> FocusResult {
            FakePlatform.focus_session_window(pid)
        }
    };
}

impl Platform for EnvPlatform {
    fn spawn_hidden(&self, cmd: &mut Command) {
        for (k, v) in &self.0 {
            cmd.env(k, v);
        }
    }
    fn kill_tree(&self, pid: u32) -> std::io::Result<()> {
        FakePlatform.kill_tree(pid)
    }
    delegate_rest_to_fake!();
}

/// A tree kill that reports success but kills nothing (ruling R9).
struct NoopKillPlatform(HashMap<String, String>);

impl Platform for NoopKillPlatform {
    fn spawn_hidden(&self, cmd: &mut Command) {
        for (k, v) in &self.0 {
            cmd.env(k, v);
        }
    }
    fn kill_tree(&self, _pid: u32) -> std::io::Result<()> {
        Ok(())
    }
    delegate_rest_to_fake!();
}

fn fake_cli(env: &[(&str, &str)]) -> RealClaudeCli {
    RealClaudeCli {
        exe: PathBuf::from(env!("CARGO_BIN_EXE_fake-claude")),
        platform: Arc::new(EnvPlatform(
            env.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )),
    }
}

fn json_file(dir: &Path, name: &str, content: &str) -> String {
    let p = dir.join(name);
    std::fs::write(&p, content).unwrap();
    p.to_string_lossy().to_string()
}

fn text(r: Result<Vec<LiveSession>, AgentsError>) -> String {
    r.expect_err("expected an error").to_string()
}

#[test]
fn real_cli_reads_agents_json() {
    let t = tmp("hub-agents-");
    let f = json_file(
        t.path(),
        "ok.json",
        &json!([{ "pid": 9, "sessionId": "z", "status": "busy" }]).to_string(),
    );
    let r = fake_cli(&[("FAKE_CLAUDE_MODE", "ok"), ("FAKE_AGENTS_JSON", &f)])
        .agents(t.path(), &StopToken::new())
        .unwrap();
    assert_eq!(
        r.iter().map(|s| (s.pid, s.status)).collect::<Vec<_>>(),
        [(9, LiveStatus::Busy)]
    );
}

#[test]
fn real_cli_non_zero_exit_gives_exact_message_without_output_text() {
    let t = tmp("hub-agents-");
    let e = fake_cli(&[
        ("FAKE_CLAUDE_MODE", "exit"),
        ("FAKE_AGENTS_EXIT", "3"),
        ("FAKE_AGENTS_MARKER", "SECRETMARK"),
    ])
    .agents(t.path(), &StopToken::new());
    let msg = text(e);
    assert_eq!(msg, "claude agents exited 3");
    assert!(!msg.contains("SECRETMARK"));
}

#[test]
fn real_cli_child_gets_claude_config_dir_profile_dir() {
    let t = tmp("hub-agents-");
    let r = fake_cli(&[("FAKE_CLAUDE_MODE", "echo-config")])
        .agents(t.path(), &StopToken::new())
        .unwrap();
    assert_eq!(
        r.iter().map(|s| s.cwd.clone()).collect::<Vec<_>>(),
        [t.path().to_string_lossy().to_string()]
    );
}

#[test]
fn real_cli_output_over_4_mib_is_refused_not_buffered() {
    let t = tmp("hub-agents-");
    // Valid JSON padded with spaces, so only the size cap can refuse it.
    let e = fake_cli(&[("FAKE_CLAUDE_MODE", "oversize")]).agents(t.path(), &StopToken::new());
    assert_eq!(text(e), "claude agents output is over 4 MiB");
}

#[test]
fn real_cli_a_stop_kills_the_child_and_returns_at_once() {
    let t = tmp("hub-agents-");
    let stop = StopToken::new();
    let s2 = stop.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        s2.stop();
    });
    let t0 = Instant::now();
    let e = fake_cli(&[("FAKE_CLAUDE_MODE", "sleep-forever")]).agents_with_timeout(
        t.path(),
        &stop,
        Duration::from_secs(20),
    );
    assert_eq!(text(e), "claude agents aborted");
    assert!(t0.elapsed() < Duration::from_secs(5));
}

#[test]
fn real_cli_an_already_stopped_token_spawns_nothing() {
    let t = tmp("hub-agents-");
    let stop = StopToken::new();
    stop.stop();
    let cli = RealClaudeCli {
        exe: PathBuf::from("no-such-binary-xyz"),
        platform: Arc::new(FakePlatform),
    };
    assert_eq!(text(cli.agents(t.path(), &stop)), "claude agents aborted");
}

#[test]
fn real_cli_a_missing_binary_is_a_spawn_error() {
    let t = tmp("hub-agents-");
    let cli = RealClaudeCli {
        exe: t.path().join("no-such-binary-xyz"),
        platform: Arc::new(FakePlatform),
    };
    let e = cli.agents(t.path(), &StopToken::new()).unwrap_err();
    assert!(matches!(e, AgentsError::Spawn(_)));
    assert!(e.to_string().starts_with("claude agents could not start: "));
}

#[test]
fn real_cli_bad_json_and_non_array_error() {
    let t = tmp("hub-agents-");
    for mode in ["garbage", "non-array"] {
        let e = fake_cli(&[("FAKE_CLAUDE_MODE", mode)]).agents(t.path(), &StopToken::new());
        assert_eq!(text(e), "claude agents returned invalid JSON");
    }
}

/// Ruling R9: the root is killed directly after the tree kill, even when the tree kill returned Ok
/// without killing anything, so reaping it cannot block (the fake would sleep 30 s).
#[test]
fn real_cli_kill_still_ends_the_root_when_kill_tree_reports_ok_but_kills_nothing() {
    let t = tmp("hub-agents-");
    let cli = RealClaudeCli {
        exe: PathBuf::from(env!("CARGO_BIN_EXE_fake-claude")),
        platform: Arc::new(NoopKillPlatform(HashMap::from([(
            "FAKE_CLAUDE_MODE".to_string(),
            "sleep-forever".to_string(),
        )]))),
    };
    let t0 = Instant::now();
    let e = cli.agents_with_timeout(t.path(), &StopToken::new(), Duration::from_millis(300));
    assert_eq!(text(e), "claude agents timed out");
    assert!(
        t0.elapsed() < Duration::from_secs(5),
        "took {:?}",
        t0.elapsed()
    );
}

#[test]
fn real_cli_a_child_that_outlives_the_timeout_is_killed_and_the_call_fails() {
    let t = tmp("hub-agents-");
    let t0 = Instant::now();
    let e = fake_cli(&[("FAKE_CLAUDE_MODE", "sleep-forever")]).agents_with_timeout(
        t.path(),
        &StopToken::new(),
        Duration::from_millis(500),
    );
    assert_eq!(text(e), "claude agents timed out");
    assert!(t0.elapsed() < Duration::from_secs(5));
}

/// The tree kill is `taskkill /T`; on Windows a grandchild holding stdout must not block the call.
#[cfg(windows)]
#[test]
fn real_cli_timeout_returns_promptly_even_if_a_grandchild_holds_stdout_and_kills_it() {
    let t = tmp("hub-agents-");
    let pid_file = t.path().join("gc.pid");
    let t0 = Instant::now();
    let e = fake_cli(&[
        ("FAKE_CLAUDE_MODE", "sleep-forever"),
        (
            "FAKE_AGENTS_GRANDCHILD_PID_FILE",
            &pid_file.to_string_lossy(),
        ),
    ])
    .agents_with_timeout(t.path(), &StopToken::new(), Duration::from_secs(4));
    assert_eq!(text(e), "claude agents timed out");
    assert!(t0.elapsed() < Duration::from_secs(8));
    let mut gc = String::new();
    for _ in 0..30 {
        gc = std::fs::read_to_string(&pid_file).unwrap_or_default();
        if !gc.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!gc.is_empty(), "grandchild pid file");
    let mut alive = true;
    for _ in 0..30 {
        let out = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {gc}"), "/NH"])
            .output()
            .unwrap();
        alive = String::from_utf8_lossy(&out.stdout).contains(&gc);
        if !alive {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!alive, "grandchild must be gone");
}

// ---------------------------------------------------------------- watch_sessions

struct Profile {
    root: TempDir,
    sessions: PathBuf,
}

fn profile() -> Profile {
    let root = tmp("hub-watch-");
    let sessions = root.path().join("sessions");
    std::fs::create_dir(&sessions).unwrap();
    Profile { root, sessions }
}

fn counter() -> (Arc<AtomicUsize>, impl Fn() + Send + 'static) {
    let n = Arc::new(AtomicUsize::new(0));
    let n2 = n.clone();
    (n, move || {
        n2.fetch_add(1, Ordering::SeqCst);
    })
}

fn settle() {
    std::thread::sleep(Duration::from_millis(DEBOUNCE_MS + 600));
}

/// Polls until `n` reaches `want` (or 5 s pass), then lets one more debounce window go by.
fn await_count(n: &AtomicUsize, want: usize) {
    let t0 = Instant::now();
    while n.load(Ordering::SeqCst) < want && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    settle();
}

fn write(dir: &Path, name: &str) {
    std::fs::write(dir.join(name), "{}").unwrap();
}

#[test]
fn watch_one_call_per_burst_of_json_writes() {
    let p = profile();
    let (n, f) = counter();
    let h = watch_sessions(p.root.path(), f).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    for i in 0..5 {
        write(&p.sessions, &format!("{i}.json"));
    }
    await_count(&n, 1);
    drop(h);
    assert_eq!(n.load(Ordering::SeqCst), 1);
}

#[test]
fn watch_ignores_key_and_other_files() {
    let p = profile();
    let (n, f) = counter();
    let h = watch_sessions(p.root.path(), f).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    write(&p.sessions, "12.abcd.key");
    write(&p.sessions, "notes.txt");
    settle();
    assert_eq!(n.load(Ordering::SeqCst), 0);
    write(&p.sessions, "13.json");
    await_count(&n, 1);
    drop(h);
    assert_eq!(n.load(Ordering::SeqCst), 1);
}

#[test]
fn watch_never_touches_key_files() {
    // Only the name decides, and a `.key` never fires.
    assert!(!should_fire(Some("x.key")));
    assert!(!should_fire(Some("12.abcd.key")));
    assert!(should_fire(Some("1.json")));
    assert!(should_fire(None));
    // The watcher module never opens, reads or stats a file.
    let src = include_str!("../src/discovery/watch.rs");
    let code: String = src
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    for banned in [
        "File::open",
        "fs::read",
        "fs::metadata",
        "OpenOptions",
        ".metadata(",
    ] {
        assert!(!code.contains(banned), "watch.rs must not use {banned}");
    }
    // A changed key file never reaches the callback.
    let p = profile();
    std::fs::write(p.sessions.join("x.key"), "a").unwrap();
    let (n, f) = counter();
    let h = watch_sessions(p.root.path(), f).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    std::fs::write(p.sessions.join("x.key"), "b").unwrap();
    std::fs::remove_file(p.sessions.join("x.key")).unwrap();
    settle();
    drop(h);
    assert_eq!(n.load(Ordering::SeqCst), 0);
}

#[test]
fn watch_should_fire_null_and_json_fire_others_do_not() {
    assert!(should_fire(None));
    assert!(should_fire(Some("1.json")));
    assert!(!should_fire(Some("1.abc.key")));
    assert!(!should_fire(Some("notes.txt")));
    assert!(!should_fire(Some("json")));
}

#[test]
fn watch_drop_cancels_a_pending_call() {
    let p = profile();
    let (n, f) = counter();
    let h = watch_sessions(p.root.path(), f).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    write(&p.sessions, "1.json");
    std::thread::sleep(Duration::from_millis(100));
    drop(h);
    settle();
    assert_eq!(n.load(Ordering::SeqCst), 0);
}

#[test]
fn watch_missing_folder_is_an_error() {
    let p = profile();
    let e = watch_sessions(&p.root.path().join("nope"), || {})
        .err()
        .unwrap();
    assert_eq!(e.to_string(), "sessions folder is not a directory");
    // A file named sessions is not a folder either.
    let q = tmp("hub-watch-");
    std::fs::write(q.path().join("sessions"), "x").unwrap();
    assert!(watch_sessions(q.path(), || {}).is_err());
}

#[test]
fn watch_a_panicking_on_change_is_routed_to_on_error() {
    let p = profile();
    let errs: Arc<Mutex<Vec<String>>> = Arc::default();
    let e2 = errs.clone();
    let h = watch_sessions_with(
        p.root.path(),
        || panic!("boom with message text"),
        move |m| e2.lock().unwrap().push(m),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    write(&p.sessions, "1.json");
    let t0 = Instant::now();
    while errs.lock().unwrap().is_empty() && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(h);
    // Fixed text: the payload may carry anything the callback held (it reaches daemon.log).
    assert_eq!(*errs.lock().unwrap(), ["on_change panicked"]);
}

#[test]
fn watch_keeps_firing_after_a_panicking_call() {
    let p = profile();
    let n = Arc::new(AtomicUsize::new(0));
    let n2 = n.clone();
    let h = watch_sessions(p.root.path(), move || {
        if n2.fetch_add(1, Ordering::SeqCst) == 0 {
            panic!("first call fails");
        }
    })
    .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    write(&p.sessions, "1.json");
    await_count(&n, 1);
    write(&p.sessions, "2.json");
    await_count(&n, 2);
    drop(h);
    assert_eq!(n.load(Ordering::SeqCst), 2);
}
