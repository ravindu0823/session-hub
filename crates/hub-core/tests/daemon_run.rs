//! Port of the `daemonRun (foreground)` tests in `test/cli.test.ts`, plus the Rust-only rules: the
//! panic hook (R13a), the refusal without an own start time (R13b), the expiry task and the
//! delivery checker wiring. A test binary of its own: the panic hook is process-wide, and these
//! tests run one at a time.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use hub_core::config::{Config, DAEMON_STOPPED_FILE};
use hub_core::daemon::collector::{DaemonDeps, DaemonError, WatchFn};
use hub_core::daemon::lock::{DaemonIdentity, LOCK_FILE, LockInfo};
use hub_core::daemon::log::DAEMON_LOG;
use hub_core::daemon::run::{DaemonRunDeps, RunOutcome, daemon_run};
use hub_core::discovery::{AgentsError, ClaudeCli, LiveSession, StopToken, WatchHandle};
use hub_core::ingest::records::Classified;
use hub_core::platform::{FocusResult, Platform, SignalGuard};
use hub_core::send::{build_message, sha256_hex};
use hub_core::store::{SendRow, SendState, Store, iso_utc_ms};
use tempfile::TempDir;

const T0: i64 = 1_763_000_000_000;
const OWN_START: i64 = 1_700_000_000_000;
const S1: &str = "11111111-2222-4333-8444-555555555555";

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

struct NoAgents;

impl ClaudeCli for NoAgents {
    fn agents(&self, _: &Path, _: &StopToken) -> Result<Vec<LiveSession>, AgentsError> {
        Ok(Vec::new())
    }
}

/// Records calls; `raise` plays a Ctrl-C / SIGTERM to every installed handler.
struct TestPlatform {
    own_start: Option<i64>,
    calls: Mutex<Vec<String>>,
    handlers: Arc<Mutex<Vec<StopToken>>>,
    removed: Arc<AtomicUsize>,
}

impl TestPlatform {
    fn new(own_start: Option<i64>) -> Arc<TestPlatform> {
        Arc::new(TestPlatform {
            own_start,
            calls: Mutex::new(Vec::new()),
            handlers: Arc::default(),
            removed: Arc::default(),
        })
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn raise(&self) {
        for s in self.handlers.lock().unwrap().iter() {
            s.stop();
        }
    }
    fn installed(&self) -> usize {
        self.handlers.lock().unwrap().len()
    }
}

impl Platform for TestPlatform {
    fn ensure_private_dir(&self, dir: &Path) -> std::io::Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("ensure_private_dir {}", dir.display()));
        fs::create_dir_all(dir)
    }
    fn is_private_dir(&self, _: &Path) -> std::io::Result<bool> {
        Ok(true)
    }
    fn process_alive(&self, pid: u32) -> bool {
        pid == std::process::id()
    }
    fn process_start_time(&self, pid: u32) -> Option<i64> {
        (pid == std::process::id())
            .then_some(self.own_start)
            .flatten()
    }
    fn process_image_path(&self, _: u32) -> Option<PathBuf> {
        None
    }
    fn spawn_hidden(&self, _: &mut Command) {}
    fn kill_tree(&self, _: u32) -> std::io::Result<()> {
        unreachable!()
    }
    fn focus_session_window(&self, _: u32) -> FocusResult {
        FocusResult::Unsupported
    }
    fn stop_on_signals(&self, stop: StopToken) -> std::io::Result<SignalGuard> {
        self.handlers.lock().unwrap().push(stop);
        let handlers = self.handlers.clone();
        let removed = self.removed.clone();
        Ok(SignalGuard::new(move || {
            handlers.lock().unwrap().clear();
            removed.fetch_add(1, SeqCst);
        }))
    }
}

struct Env {
    _root: TempDir,
    config: Config,
    platform: Arc<TestPlatform>,
}

fn env() -> Env {
    env_with(Some(OWN_START))
}

fn env_with(own_start: Option<i64>) -> Env {
    let root = tempfile::Builder::new()
        .prefix("hub-daemon-run-")
        .tempdir()
        .unwrap();
    let config = Config {
        profiles: Vec::new(),
        data_dir: root.path().join("data"),
    };
    Env {
        _root: root,
        config,
        platform: TestPlatform::new(own_start),
    }
}

fn no_watch() -> WatchFn {
    Box::new(|_: &Path, _, _| Ok(WatchHandle::from_drop(|| {})))
}

type Run = Box<dyn FnOnce(DaemonDeps) -> Result<(), DaemonError> + Send>;

impl Env {
    fn deps(
        &self,
        identify: impl Fn(&LockInfo) -> DaemonIdentity + Send + Sync + 'static,
        run: Option<Run>,
    ) -> DaemonRunDeps {
        DaemonRunDeps {
            config: self.config.clone(),
            platform: self.platform.clone(),
            cli: Arc::new(NoAgents),
            now: Arc::new(|| T0),
            daemon_identity: Arc::new(identify),
            watch: no_watch(),
            run_daemon: run,
        }
    }

    fn write_lock(&self, text: &str) {
        let run = self.config.run_dir();
        fs::create_dir_all(&run).unwrap();
        fs::write(run.join(LOCK_FILE), text).unwrap();
    }

    fn log(&self) -> String {
        fs::read_to_string(self.config.run_dir().join(DAEMON_LOG)).unwrap_or_default()
    }
}

fn foreign(_: &LockInfo) -> DaemonIdentity {
    DaemonIdentity::Foreign
}

/// What an injected collector saw. An assert inside it cannot fail a test: `daemon_run` catches
/// the panic and returns `Stopped`. So the closures only write notes, and the test asserts on them
/// after `daemon_run` returns.
type Notes = Arc<Mutex<Vec<String>>>;

fn read(notes: &Notes) -> Vec<String> {
    notes.lock().unwrap().clone()
}

/// A stand-in collector: notes whether the token was already stopped, plays a signal, and notes
/// whether that stopped it.
fn until_signal(platform: Arc<TestPlatform>, notes: Notes) -> Option<Run> {
    Some(Box::new(move |d: DaemonDeps| {
        let mut n = vec![format!("running stopped={}", d.stop.is_stopped())];
        platform.raise();
        let stopped = d.stop.wait_timeout(Duration::from_secs(2));
        n.push(format!("signal stopped={stopped}"));
        notes.lock().unwrap().extend(n);
        Ok(())
    }))
}

const SIGNALLED: [&str; 2] = ["running stopped=false", "signal stopped=true"];

/// `Stopped`, and no panic was caught on the way (a failed check in an injected collector panics).
fn assert_stopped_clean(e: &Env, out: &RunOutcome) {
    assert_eq!(*out, RunOutcome::Stopped);
    let log = e.log();
    assert!(!log.contains(r#""event":"panic""#), "{log}");
}

#[test]
fn already_running_when_a_live_daemon_holds_the_lock() {
    let _s = serial();
    let e = env();
    e.write_lock("66");
    let ran = Arc::new(AtomicUsize::new(0));
    let r = ran.clone();
    let out = daemon_run(e.deps(
        |l| {
            if l.pid == 66 {
                DaemonIdentity::Ours
            } else {
                DaemonIdentity::Foreign
            }
        },
        Some(Box::new(move |_| {
            r.fetch_add(1, SeqCst);
            Ok(())
        })),
    ));
    assert_eq!(out, RunOutcome::AlreadyRunning);
    assert_eq!(ran.load(SeqCst), 0);
    assert!(e.platform.calls().is_empty());
}

#[test]
fn a_lock_naming_another_program_or_a_reused_pid_does_not_stop_the_start() {
    let _s = serial();
    for lock in ["66", &format!("66 {T0}")] {
        let e = env();
        e.write_lock(lock);
        let notes = Notes::default();
        let out = daemon_run(e.deps(foreign, until_signal(e.platform.clone(), notes.clone())));
        assert_stopped_clean(&e, &out);
        assert_eq!(read(&notes), SIGNALLED, "{lock}");
    }
}

#[test]
fn a_lock_naming_a_live_pid_that_cannot_be_verified_means_already_running() {
    let _s = serial();
    let e = env();
    e.write_lock("66");
    let notes = Notes::default();
    let out = daemon_run(e.deps(
        |_| DaemonIdentity::Unknown,
        until_signal(e.platform.clone(), notes.clone()),
    ));
    assert_eq!(out, RunOutcome::AlreadyRunning);
    assert!(read(&notes).is_empty());
}

#[test]
fn a_store_that_cannot_open_is_logged_before_failing() {
    let _s = serial();
    let e = env();
    fs::create_dir_all(e.config.db_path()).unwrap(); // a folder where the index file should be
    let out = daemon_run(e.deps(foreign, None));
    assert!(matches!(out, RunOutcome::Failed(_)), "{out:?}");
    assert!(e.log().contains(r#""event":"store_open_failed""#));
    assert_eq!(e.platform.installed(), 0);
}

#[test]
fn a_panic_is_logged_without_its_payload_and_stops_the_daemon_cleanly() {
    // R13a: the hook logs a fixed event and the location only; a payload could quote record text.
    let _s = serial();
    let e = env();
    let out = daemon_run(e.deps(
        foreign,
        Some(Box::new(|_| panic!("SECRET-PAYLOAD fixture owner prompt"))),
    ));
    assert_eq!(out, RunOutcome::Stopped);
    let log = e.log();
    assert!(log.contains(r#""level":"error""#), "{log}");
    assert!(log.contains(r#""event":"panic""#), "{log}");
    assert!(log.contains("daemon_run.rs:"), "{log}");
    assert!(!log.contains("SECRET-PAYLOAD"), "{log}");
    assert_eq!(e.platform.installed(), 0);
    assert_eq!(e.platform.removed.load(SeqCst), 1);
    // The previous hook is back: a later panic is not written to this daemon's log.
    let before = log.matches(r#""event":"panic""#).count();
    let _ = std::panic::catch_unwind(|| panic!("after the run"));
    assert_eq!(e.log().matches(r#""event":"panic""#).count(), before);
}

#[test]
fn a_lost_lock_race_inside_run_daemon_is_already_running() {
    let _s = serial();
    let e = env();
    let out = daemon_run(e.deps(
        foreign,
        Some(Box::new(|_| Err(DaemonError::AlreadyRunning))),
    ));
    assert_eq!(out, RunOutcome::AlreadyRunning);
}

#[test]
fn any_other_run_daemon_error_is_logged_and_failed() {
    let _s = serial();
    let e = env();
    let out = daemon_run(e.deps(foreign, Some(Box::new(|_| Err(DaemonError::NotPrivate)))));
    assert_eq!(out, RunOutcome::Failed("data dir is not private".into()));
    assert!(e.log().contains(r#""event":"daemon_failed""#));
}

#[test]
fn a_daemon_that_runs_clears_the_stop_marker() {
    let _s = serial();
    let e = env();
    let run = e.config.run_dir();
    fs::create_dir_all(&run).unwrap();
    fs::write(run.join(DAEMON_STOPPED_FILE), "").unwrap();
    let notes = Notes::default();
    let out = daemon_run(e.deps(foreign, until_signal(e.platform.clone(), notes.clone())));
    assert_stopped_clean(&e, &out);
    assert_eq!(read(&notes), SIGNALLED);
    assert!(!run.join(DAEMON_STOPPED_FILE).exists());
}

#[test]
fn sets_up_dirs_runs_the_collector_until_a_signal_then_cleans_up() {
    let _s = serial();
    let e = env();
    type Seen = Arc<Mutex<Option<(PathBuf, PathBuf, bool, bool)>>>;
    let seen: Seen = Arc::default();
    let s = seen.clone();
    let notes = Notes::default();
    let n = notes.clone();
    let platform = e.platform.clone();
    let out = daemon_run(e.deps(
        foreign,
        Some(Box::new(move |d: DaemonDeps| {
            *s.lock().unwrap() = Some((
                d.data_dir.clone(),
                d.relay_dir.clone(),
                d.on_records.is_some(),
                d.expire.is_some(),
            ));
            n.lock()
                .unwrap()
                .push(format!("handlers={}", platform.installed()));
            platform.raise();
            let stopped = d.stop.wait_timeout(Duration::from_secs(2));
            n.lock().unwrap().push(format!("signal stopped={stopped}"));
            Ok(())
        })),
    ));
    assert_stopped_clean(&e, &out);
    assert_eq!(read(&notes), ["handlers=1", "signal stopped=true"]);
    let c = &e.config;
    assert_eq!(
        e.platform.calls().first().map(String::as_str),
        Some(format!("ensure_private_dir {}", c.data_dir.display()).as_str())
    );
    assert!(c.run_dir().is_dir());
    assert!(c.relay_dir().is_dir());
    assert!(c.db_path().is_file());
    let seen = seen.lock().unwrap().clone().unwrap();
    assert_eq!(seen, (c.data_dir.clone(), c.relay_dir(), true, true));
    assert_eq!(e.platform.installed(), 0);
    assert_eq!(e.platform.removed.load(SeqCst), 1);
    // The store was closed: the db file can be removed on Windows.
    fs::remove_dir_all(&c.data_dir).unwrap();
    assert!(!c.data_dir.exists());
}

#[test]
fn own_start_time_unavailable_is_logged_and_failed_with_no_lock() {
    // R13b, through the real run_daemon.
    let _s = serial();
    let e = env_with(None);
    let out = daemon_run(e.deps(foreign, None));
    assert!(matches!(out, RunOutcome::Failed(_)), "{out:?}");
    let log = e.log();
    assert!(log.contains(r#""level":"error""#), "{log}");
    assert!(log.contains(r#""event":"daemon_failed""#), "{log}");
    assert!(!e.config.run_dir().join(LOCK_FILE).exists());
}

#[test]
fn the_real_collector_runs_until_a_signal_and_releases_the_lock() {
    let _s = serial();
    let e = env();
    let platform = e.platform.clone();
    let lock = e.config.run_dir().join(LOCK_FILE);
    let watcher = std::thread::spawn(move || {
        // Plays Ctrl-C once the daemon holds its lock.
        for _ in 0..400 {
            if platform.installed() > 0 && lock.exists() {
                platform.raise();
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    });
    let out = daemon_run(e.deps(foreign, None));
    assert!(watcher.join().unwrap());
    assert_eq!(out, RunOutcome::Stopped);
    assert!(!e.config.run_dir().join(LOCK_FILE).exists());
    let log = e.log();
    assert!(log.contains(r#""event":"start""#), "{log}");
    assert!(log.contains(r#""event":"stop""#), "{log}");
}

fn pending_send(store: &Store, text: &str, created_at: i64) -> i64 {
    store
        .create_send(&SendRow {
            id: None,
            session_id: S1.into(),
            profile: "claude".into(),
            name: "n".into(),
            sha256: sha256_hex(&build_message(text)),
            created_at,
            state: SendState::Pending,
            detail: None,
        })
        .unwrap()
}

fn state(store: &Store, id: i64) -> SendState {
    store.get_send(id).unwrap().unwrap().state
}

#[test]
fn the_expire_task_expires_overdue_sends_and_logs_the_count() {
    let _s = serial();
    let e = env();
    let seen: Arc<Mutex<Vec<SendState>>> = Arc::default();
    let s = seen.clone();
    let out = daemon_run(e.deps(
        foreign,
        Some(Box::new(move |mut d: DaemonDeps| {
            let old = pending_send(&d.store, "old", T0 - 31 * 60_000);
            let fresh = pending_send(&d.store, "fresh", T0 - 60_000);
            let Some(expire) = d.expire.as_mut() else {
                return Ok(());
            };
            expire(&d.store);
            // Nothing more to expire: no line.
            expire(&d.store);
            s.lock()
                .unwrap()
                .extend([state(&d.store, old), state(&d.store, fresh)]);
            Ok(())
        })),
    ));
    assert_stopped_clean(&e, &out);
    assert_eq!(
        *seen.lock().unwrap(),
        [SendState::Expired, SendState::Pending]
    );
    let log = e.log();
    assert_eq!(
        log.matches(r#""event":"sends_expired""#).count(),
        1,
        "{log}"
    );
    assert!(log.contains(r#""count":1"#), "{log}");
}

#[test]
fn an_expire_store_error_is_logged_as_expire_failed() {
    // R8: expire_sends returns the store error; the daemon logs it (warn) and keeps running.
    let _s = serial();
    let e = env();
    let out = daemon_run(e.deps(
        foreign,
        Some(Box::new(|mut d: DaemonDeps| {
            pending_send(&d.store, "old", T0 - 31 * 60_000);
            let db = d.data_dir.join("hub.db");
            rusqlite::Connection::open(db)
                .unwrap()
                .execute_batch(
                    "CREATE TRIGGER test_fail BEFORE UPDATE ON sends
                     BEGIN SELECT RAISE(ABORT, 'refused'); END;",
                )
                .unwrap();
            (d.expire.as_mut().unwrap())(&d.store);
            Ok(())
        })),
    ));
    assert_stopped_clean(&e, &out);
    let log = e.log();
    let line = log
        .lines()
        .find(|l| l.contains(r#""event":"expire_failed""#))
        .unwrap_or_else(|| panic!("{log}"));
    assert!(line.contains(r#""level":"warn""#), "{line}");
    assert!(line.contains(r#""err":"Error""#), "{line}");
}

#[test]
fn on_records_is_the_delivery_checker() {
    let _s = serial();
    let e = env();
    let notes = Notes::default();
    let n = notes.clone();
    let out = daemon_run(e.deps(
        foreign,
        Some(Box::new(move |mut d: DaemonDeps| {
            let id = pending_send(&d.store, "deploy it", T0 - 5_000);
            let record = Classified::PeerMessage {
                uuid: "p1".into(),
                ts: iso_utc_ms(T0 - 1_000),
                from_name: "session-hub-relay".into(),
                from_mode: "default".into(),
                body: build_message("deploy it"),
            };
            let Some(hook) = d.on_records.as_mut() else {
                return Ok(());
            };
            let res = hook(&d.store, S1, &[record]);
            let after = state(&d.store, id);
            n.lock()
                .unwrap()
                .push(format!("ok={} state={}", res.is_ok(), after.as_str()));
            Ok(())
        })),
    ));
    assert_stopped_clean(&e, &out);
    assert_eq!(read(&notes), ["ok=true state=delivered"]);
}
