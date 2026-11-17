//! End-to-end tests of the `hub` binary. Every spawned hub gets temp HOME / LOCALAPPDATA / XDG
//! dirs, so nothing touches the real data dir or `~/.claude*`; the fake `claude` sits first on PATH.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use hub_core::config::{current_os, resolve_config};

const HUB: &str = env!("CARGO_BIN_EXE_hub");

struct Env {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    bin: PathBuf,
    run_dir: PathBuf,
}

impl Env {
    fn new() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let bin = tmp.path().join("bin");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&bin).unwrap();
        let fake =
            Path::new(HUB).with_file_name(format!("fake-claude{}", std::env::consts::EXE_SUFFIX));
        assert!(
            fake.exists(),
            "{} is missing: run `cargo test --workspace` (it builds hub-core's fake-claude)",
            fake.display()
        );
        fs::copy(
            &fake,
            bin.join(format!("claude{}", std::env::consts::EXE_SUFFIX)),
        )
        .unwrap();
        let mut e = Env {
            _tmp: tmp,
            home,
            bin,
            run_dir: PathBuf::new(),
        };
        let config = resolve_config(&|n| e.var(n), current_os()).unwrap();
        e.run_dir = config.run_dir();
        e
    }

    fn var(&self, name: &str) -> Option<String> {
        let h = self.home.to_str().unwrap();
        match name {
            "HOME" | "USERPROFILE" => Some(h.to_string()),
            "LOCALAPPDATA" => Some(format!("{h}/AppData/Local")),
            "XDG_DATA_HOME" => Some(format!("{h}/xdg")),
            _ => None,
        }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        self.cmd_of(HUB, args)
    }

    /// `exe` with the temp environment (`hub` or `hubw`).
    fn cmd_of(&self, exe: &str, args: &[&str]) -> Command {
        let mut c = Command::new(exe);
        c.args(args)
            .env_clear()
            .env("PATH", &self.bin)
            .stdin(Stdio::null());
        for n in ["HOME", "USERPROFILE", "LOCALAPPDATA", "XDG_DATA_HOME"] {
            c.env(n, self.var(n).unwrap());
        }
        // Windows needs these to start a process at all.
        for n in ["SystemRoot", "SYSTEMROOT", "TEMP", "TMP"] {
            if let Some(v) = std::env::var_os(n) {
                c.env(n, v);
            }
        }
        c
    }

    fn hub(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    /// Stops the daemon when dropped, even when the test failed.
    fn guard(&self) -> StopGuard<'_> {
        StopGuard(self)
    }
}

struct StopGuard<'a>(&'a Env);

impl Drop for StopGuard<'_> {
    fn drop(&mut self) {
        let _ = self.0.hub(&["daemon", "stop"]);
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).trim().to_string()
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap()
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out: {what}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A long-lived non-hub process (the platform's own `sleep`-like helper).
fn spawn_sleeper() -> Child {
    #[cfg(unix)]
    let c = Command::new("sleep").arg("60").spawn();
    #[cfg(windows)]
    let c = Command::new("ping")
        .args(["-n", "60", "127.0.0.1"])
        .stdout(Stdio::null())
        .spawn();
    c.unwrap()
}

#[test]
fn status_stopped_exit_3() {
    let e = Env::new();
    let o = e.hub(&["daemon", "status"]);
    assert_eq!((code(&o), text(&o.stdout)), (3, "STOPPED".to_string()));
}

#[test]
fn start_then_status_running_then_stop() {
    let e = Env::new();
    let _g = e.guard();
    // A stale deliberate stop is ended by start.
    fs::create_dir_all(&e.run_dir).unwrap();
    fs::write(e.run_dir.join("daemon.stopped"), "").unwrap();

    let o = e.hub(&["daemon", "start"]);
    let out = text(&o.stdout);
    assert_eq!(code(&o), 0, "{out} {}", text(&o.stderr));
    let pid: u32 = out
        .strip_prefix("STARTED pid=")
        .expect(&out)
        .parse()
        .unwrap();
    assert!(!e.run_dir.join("daemon.stopped").exists());

    let o = e.hub(&["daemon", "status"]);
    assert_eq!(
        (code(&o), text(&o.stdout)),
        (0, format!("RUNNING pid={pid}"))
    );

    let o = e.hub(&["daemon", "start"]);
    assert_eq!(
        (code(&o), text(&o.stdout)),
        (0, format!("ALREADY RUNNING pid={pid}"))
    );

    let o = e.hub(&["daemon", "stop"]);
    assert_eq!((code(&o), text(&o.stdout)), (0, "STOPPED".to_string()));
    assert!(!hub_platform::native().process_alive(pid));
    let o = e.hub(&["daemon", "status"]);
    assert_eq!((code(&o), text(&o.stdout)), (3, "STOPPED".to_string()));
}

#[test]
fn stop_writes_stopped_file_first() {
    let e = Env::new();
    // Not running: the marker is still written, so a bridge does not start the daemon.
    let o = e.hub(&["daemon", "stop"]);
    assert_eq!((code(&o), text(&o.stdout)), (0, "STOPPED".to_string()));
    assert!(e.run_dir.join("daemon.stopped").exists());

    // Running: after the stop the marker exists and the daemon is gone.
    let _g = e.guard();
    assert_eq!(code(&e.hub(&["daemon", "start"])), 0);
    assert!(!e.run_dir.join("daemon.stopped").exists());
    assert_eq!(code(&e.hub(&["daemon", "stop"])), 0);
    assert!(e.run_dir.join("daemon.stopped").exists());
}

#[test]
fn stop_refuses_foreign_pid() {
    let e = Env::new();
    let mut sleeper = spawn_sleeper();
    let pid = sleeper.id();
    fs::create_dir_all(&e.run_dir).unwrap();

    // A legacy lock (`<pid>`) is judged by image: not a daemon, so stale, not killed.
    fs::write(e.run_dir.join("daemon.lock"), pid.to_string()).unwrap();
    let o = e.hub(&["daemon", "stop"]);
    assert_eq!((code(&o), text(&o.stdout)), (0, "STOPPED".to_string()));
    assert!(hub_platform::native().process_alive(pid));

    // A lock with another start time: a reused pid, also stale and left alone.
    fs::write(e.run_dir.join("daemon.lock"), format!("{pid} 1000")).unwrap();
    let o = e.hub(&["daemon", "stop"]);
    assert_eq!((code(&o), text(&o.stdout)), (0, "STOPPED".to_string()));
    assert!(hub_platform::native().process_alive(pid));

    // Our start time but an image that is no daemon's: held, never killed, reported as failed.
    let started = hub_platform::native().process_start_time(pid).unwrap();
    fs::write(e.run_dir.join("daemon.lock"), format!("{pid} {started}")).unwrap();
    let o = e.hub(&["daemon", "stop"]);
    assert_eq!(code(&o), 1);
    assert!(text(&o.stderr).contains("cannot be verified as the daemon"));
    assert!(hub_platform::native().process_alive(pid));

    sleeper.kill().unwrap();
    sleeper.wait().unwrap();
}

#[test]
fn run_twice_second_reports_already_running() {
    let e = Env::new();
    let _g = e.guard();
    let mut first = e
        .cmd(&["daemon", "run"])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let pid = first.id();
    wait_until("the first daemon holds the lock", || {
        fs::read_to_string(e.run_dir.join("daemon.lock"))
            .is_ok_and(|t| t.starts_with(&pid.to_string()))
    });
    let o = e.hub(&["daemon", "run"]);
    assert_eq!(
        (code(&o), text(&o.stdout)),
        (0, "ALREADY RUNNING".to_string())
    );
    // Reaped by the wait, so the stop sees it gone rather than a zombie.
    let reaper = std::thread::spawn(move || first.wait());
    assert_eq!(code(&e.hub(&["daemon", "stop"])), 0);
    reaper.join().unwrap().unwrap();
}

#[test]
fn version_and_help() {
    let e = Env::new();
    for flag in ["--version", "-v"] {
        let o = e.hub(&[flag]);
        assert_eq!(code(&o), 0);
        assert_eq!(
            text(&o.stdout),
            format!("hub {}", env!("CARGO_PKG_VERSION"))
        );
    }
    for flag in ["--help", "-h", "help"] {
        let o = e.hub(&[flag]);
        assert_eq!(code(&o), 0);
        assert_eq!(text(&o.stdout), hub::cli::USAGE);
    }
    assert!(hub::cli::USAGE.contains("hub daemon status"));
    assert!(!hub::cli::USAGE.contains("install"));
}

#[test]
fn unknown_command_exit_2() {
    let e = Env::new();
    for args in [
        // No arguments is the TUI, not a usage error: it has its own manual check.
        &["bogus"][..],
        &["daemon"],
        &["daemon", "restart"],
        &["daemon", "status", "extra"],
        &["--version", "x"],
    ] {
        let o = e.hub(args);
        assert_eq!(code(&o), 2, "{args:?}");
        assert!(text(&o.stderr).starts_with("Usage:"), "{args:?}");
        assert_eq!(text(&o.stdout), "");
    }
}

#[test]
fn tui_start_warning_names_the_daemon_log() {
    let w = hub::daemon_cmd::start_warning(Path::new("/d/run"));
    assert!(
        w.starts_with("The daemon is not running and did not start; the list may be stale. See ")
    );
    assert!(w.ends_with("daemon.log"));
}

#[test]
fn windowless_with_no_arguments_prints_nothing_and_exits_2() {
    use std::sync::{Arc, Mutex};
    let said: Arc<Mutex<Vec<String>>> = Arc::default();
    let (o, e) = (said.clone(), said.clone());
    let platform = hub_platform::native();
    let deps = hub::cli::CliDeps {
        // The TUI path would report this config error and exit 1.
        config: Err("no config in this test".into()),
        cli: Arc::new(hub_core::discovery::RealClaudeCli {
            exe: PathBuf::from("claude-not-run"),
            platform: platform.clone(),
        }),
        platform,
        exe: PathBuf::from(HUB),
        out: Box::new(move |t| o.lock().unwrap().push(t.to_string())),
        err: Box::new(move |t| e.lock().unwrap().push(t.to_string())),
    };
    assert_eq!(hub::cli::run_windowless(Vec::new(), &deps), 2);
    assert!(said.lock().unwrap().is_empty());
    // Any command is passed on unchanged.
    assert_eq!(hub::cli::run_windowless(vec!["--version".into()], &deps), 0);
    assert_eq!(
        *said.lock().unwrap(),
        [format!("hub {}", env!("CARGO_PKG_VERSION"))]
    );
}

/// `hubw` has no console: with no arguments it must not start the daemon or a TUI nobody sees.
#[cfg(windows)]
#[test]
fn hubw_with_no_arguments_exits_2_and_starts_nothing() {
    let e = Env::new();
    let o = e.cmd_of(env!("CARGO_BIN_EXE_hubw"), &[]).output().unwrap();
    assert_eq!(code(&o), 2);
    assert_eq!(
        (text(&o.stdout), text(&o.stderr)),
        (String::new(), String::new())
    );
    let s = e.hub(&["daemon", "status"]);
    assert_eq!((code(&s), text(&s.stdout)), (3, "STOPPED".to_string()));
}
