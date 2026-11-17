//! Port of `test/daemon/collector.test.ts`: the collector loop over a fake `claude agents`, a fake
//! sessions watch the test fires, a manual clock and a temp profile. The daemon runs on its own
//! thread; the test reads `hub.db` through a second connection, as the TUI would.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hub_core::config::{
    DAEMON_STOPPED_FILE, Profile, ProfileId, RELAY_NAME, RETENTION_MS, SAFETY_POLL_MS,
};
use hub_core::daemon::collector::{
    BACKOFF_MS, DaemonDeps, DaemonError, RecordsHook, StoreTask, TAIL_POLL_MS, Timing, WatchFn,
    run_daemon,
};
use hub_core::daemon::lock::{LOCK_FILE, LockOptions, acquire_lock, identify_daemon};
use hub_core::daemon::log::{
    DAEMON_LOG, LOG_MAX_BYTES, LogEntry, LogValue, Logger, MemoryLogger, create_logger,
};
use hub_core::discovery::{
    AgentsError, ClaudeCli, LiveSession, StopToken, WatchError, WatchHandle, parse_agents,
};
use hub_core::ingest::records::Classified;
use hub_core::platform::{FocusResult, Platform, SignalGuard};
use hub_core::send::parse_js_date;
use hub_core::store::{LiveStatus, SendRow, SendState, SessionRow, SessionView, Store, iso_utc_ms};
use serde_json::{Value, json};
use tempfile::TempDir;

const S1: &str = "11111111-2222-4333-8444-555555555555";
const R1: &str = "99999999-2222-4333-8444-555555555555";
const E1: &str = "eeeeeeee-2222-4333-8444-555555555555";
const O1: &str = "00000000-2222-4333-8444-555555555555";
/// This process's start time as the test platform reports it.
const OWN_START: i64 = 1_700_000_000_000;

const PROMPT: &str = "fixture owner prompt alpha";
const PHASE: &str = "fixture phase bravo";
const REMAINING: &str = "fixture remaining charlie";
const WAITING: &str = "fixture waiting delta";
const PARTIAL: &str = "fixture partial echo";
const PLAIN: &str = "fixture plain foxtrot";
const ENDED: &str = "fixture ended golf";
const TEXTS: [&str; 7] = [PROMPT, PHASE, REMAINING, WAITING, PARTIAL, PLAIN, ENDED];

fn secret() -> String {
    format!("ghp_{}", "q".repeat(36))
}

fn status_reply() -> String {
    let s = secret();
    [
        format!("**Progress: about 40% overall. Phase: {PHASE}.**"),
        String::new(),
        "Body.".into(),
        String::new(),
        "### What's remaining".into(),
        format!("1. {REMAINING} with {s}"),
        String::new(),
        "### Waiting on you".into(),
        format!("- **New:** Q1: {WAITING} token {s}"),
    ]
    .join("\n")
}

fn wall_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

// ---- fakes ----

/// `claude agents` stand-in: returns the current list (through `parse_agents`, like the real one),
/// or exits 1 while `fail` is set. `hold` keeps calls in flight until `release`.
struct FakeCli {
    state: Mutex<CliState>,
    cv: Condvar,
    clock: Arc<AtomicI64>,
}

struct CliState {
    agents: Value,
    fail: bool,
    held: bool,
    calls: Vec<i64>,
}

impl FakeCli {
    fn new(clock: Arc<AtomicI64>) -> FakeCli {
        FakeCli {
            state: Mutex::new(CliState {
                agents: json!([]),
                fail: false,
                held: false,
                calls: Vec::new(),
            }),
            cv: Condvar::new(),
            clock,
        }
    }
    fn set_agents(&self, list: Vec<Value>) {
        self.state.lock().unwrap().agents = Value::Array(list);
    }
    fn set_fail(&self, fail: bool) {
        self.state.lock().unwrap().fail = fail;
    }
    fn calls(&self) -> Vec<i64> {
        self.state.lock().unwrap().calls.clone()
    }
    fn hold(&self) {
        self.state.lock().unwrap().held = true;
    }
    fn release(&self) {
        self.state.lock().unwrap().held = false;
        self.cv.notify_all();
    }
}

impl ClaudeCli for FakeCli {
    fn agents(&self, _dir: &Path, _stop: &StopToken) -> Result<Vec<LiveSession>, AgentsError> {
        let mut s = self.state.lock().unwrap();
        s.calls.push(self.clock.load(SeqCst));
        while s.held {
            s = self.cv.wait(s).unwrap();
        }
        if s.fail {
            Err(AgentsError::Exited(1))
        } else {
            Ok(parse_agents(&s.agents))
        }
    }
}

type Handlers = Arc<Mutex<Vec<(u64, Box<dyn Fn() + Send>)>>>;
type Killer = Arc<Mutex<Vec<Arc<dyn Fn(String) + Send + Sync>>>>;

/// The sessions watch: like `watch_sessions`, a missing sessions folder is an error (unless
/// `check_dir` is off). While `dying` is set, each new watcher reports an error 10 ms after it
/// starts. `fire` calls every live watcher's `on_change`.
#[derive(Clone)]
struct FakeWatch {
    handlers: Handlers,
    next: Arc<AtomicU64>,
    arms: Arc<AtomicUsize>,
    dying: Arc<AtomicBool>,
    check_dir: bool,
    kill: Killer,
}

impl FakeWatch {
    fn new(check_dir: bool) -> FakeWatch {
        FakeWatch {
            handlers: Arc::default(),
            next: Arc::default(),
            arms: Arc::default(),
            dying: Arc::default(),
            check_dir,
            kill: Arc::default(),
        }
    }

    fn watch_fn(&self) -> WatchFn {
        let me = self.clone();
        Box::new(move |dir: &Path, on_change, on_error| {
            me.arms.fetch_add(1, SeqCst);
            if me.check_dir && !dir.join("sessions").is_dir() {
                return Err(WatchError::NotADirectory);
            }
            let id = me.next.fetch_add(1, SeqCst);
            me.handlers.lock().unwrap().push((id, on_change));
            let on_error: Arc<dyn Fn(String) + Send + Sync> = Arc::from(on_error);
            me.kill.lock().unwrap().push(on_error.clone());
            if me.dying.load(SeqCst) {
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(10));
                    on_error("watcher died".into());
                });
            }
            let handlers = me.handlers.clone();
            Ok(WatchHandle::from_drop(move || {
                handlers.lock().unwrap().retain(|(h, _)| *h != id);
            }))
        })
    }

    fn fire(&self) {
        for (_, h) in self.handlers.lock().unwrap().iter() {
            h();
        }
    }

    fn live(&self) -> usize {
        self.handlers.lock().unwrap().len()
    }

    fn arms(&self) -> usize {
        self.arms.load(SeqCst)
    }

    /// Reports an error from the n-th watcher started (None: the latest).
    fn kill(&self, n: Option<usize>) {
        let k = {
            let all = self.kill.lock().unwrap();
            n.map_or_else(|| all.last().cloned(), |i| all.get(i).cloned())
        };
        k.expect("a watcher")("watcher died".into());
    }
}

/// Like `FakePlatform`, plus: privacy is configurable, this process has a start time, and every
/// call is recorded.
struct TestPlatform {
    private: bool,
    own_start: Option<i64>,
    calls: Mutex<Vec<&'static str>>,
}

impl TestPlatform {
    fn new(private: bool, own_start: Option<i64>) -> TestPlatform {
        TestPlatform {
            private,
            own_start,
            calls: Mutex::new(Vec::new()),
        }
    }
    fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }
    fn note(&self, m: &'static str) {
        self.calls.lock().unwrap().push(m);
    }
}

impl Platform for TestPlatform {
    fn ensure_private_dir(&self, _dir: &Path) -> std::io::Result<()> {
        self.note("ensure_private_dir");
        Ok(())
    }
    fn is_private_dir(&self, _dir: &Path) -> std::io::Result<bool> {
        self.note("is_private_dir");
        Ok(self.private)
    }
    fn process_alive(&self, pid: u32) -> bool {
        pid == std::process::id()
    }
    fn process_start_time(&self, pid: u32) -> Option<i64> {
        self.note("process_start_time");
        if pid == std::process::id() {
            self.own_start
        } else {
            None
        }
    }
    fn process_image_path(&self, _pid: u32) -> Option<PathBuf> {
        None
    }
    fn spawn_hidden(&self, _cmd: &mut Command) {}
    fn kill_tree(&self, _pid: u32) -> std::io::Result<()> {
        unreachable!("the collector never kills")
    }
    fn focus_session_window(&self, _pid: u32) -> FocusResult {
        FocusResult::Unsupported
    }
    fn stop_on_signals(&self, _stop: StopToken) -> std::io::Result<SignalGuard> {
        unreachable!("run_daemon installs no signal handlers (daemon_run does)")
    }
}

// ---- transcript lines ----

static SEQ: AtomicI64 = AtomicI64::new(0);

fn ts() -> String {
    let base = parse_js_date("2026-11-13T08:00:00.000Z").unwrap();
    iso_utc_ms(base + (SEQ.fetch_add(1, SeqCst) + 1) * 1000)
}

fn stamp(at: Option<i64>) -> String {
    at.map_or_else(ts, iso_utc_ms)
}

fn user_line(uuid: &str, text: &str, at: Option<i64>) -> String {
    json!({
        "type": "user", "isSidechain": false, "uuid": uuid, "timestamp": stamp(at),
        "cwd": "C:\\fixture-cwd", "origin": {"kind": "human"},
        "message": {"role": "user", "content": text},
    })
    .to_string()
}

fn assistant_line(uuid: &str, text: &str, at: Option<i64>) -> String {
    json!({
        "type": "assistant", "isSidechain": false, "uuid": uuid, "timestamp": stamp(at),
        "message": {"role": "assistant", "content": [{"type": "text", "text": text}]},
    })
    .to_string()
}

fn mode_line(mode: &str) -> String {
    json!({"type": "permission-mode", "permissionMode": mode, "sessionId": "x"}).to_string()
}

/// A parsed record type whose shape changed: raises the format banner.
fn unknown_line() -> String {
    json!({"type": "user", "uuid": "z", "version": "9.9.9", "message": "s"}).to_string()
}

/// A record type this build does not know: diagnostics only.
fn unrecognized_line() -> String {
    json!({"type": "brand-new-record-type", "uuid": "y", "version": "9.9.8"}).to_string()
}

fn write_transcript(dir: &Path, session_id: &str, lines: &[String]) -> PathBuf {
    let p = dir.join(format!("{session_id}.jsonl"));
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    fs::write(&p, text).unwrap();
    p
}

fn append(path: &Path, text: &str) {
    OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
}

fn agent(session_id: &str, over: Value) -> Value {
    let mut v = json!({
        "pid": 4242, "cwd": "C:\\fixture", "kind": "interactive", "startedAt": 1_000,
        "sessionId": session_id, "name": format!("name-{session_id}"), "status": "idle",
    });
    if let Value::Object(o) = over {
        for (k, x) in o {
            v[k] = x;
        }
    }
    v
}

// ---- harness ----

/// Short ticks so re-arm and tail tests run in well under a second; the safety poll never fires.
fn test_timing() -> Timing {
    Timing {
        tail_poll: Duration::from_millis(50),
        safety_poll: Duration::from_secs(3600),
        expire_every: Duration::from_secs(3600),
    }
}

struct Env {
    _root: TempDir,
    root: PathBuf,
    profile_dir: PathBuf,
    proj_dir: PathBuf,
    data_dir: PathBuf,
    relay_dir: PathBuf,
    run_dir: PathBuf,
    db_path: PathBuf,
    clock: Arc<AtomicI64>,
    cli: Arc<FakeCli>,
    watch: FakeWatch,
    log: Arc<MemoryLogger>,
    platform: Arc<TestPlatform>,
    reader: Store,
}

#[derive(Default)]
struct Over {
    cli: Option<Arc<dyn ClaudeCli>>,
    store: Option<Store>,
    watch: Option<WatchFn>,
    log: Option<Arc<dyn Logger>>,
    on_records: Option<RecordsHook>,
    expire: Option<StoreTask>,
    profiles: Option<Vec<Profile>>,
    platform: Option<Arc<TestPlatform>>,
    timing: Option<Timing>,
}

struct Running {
    stop: StopToken,
    thread: Option<JoinHandle<Result<(), DaemonError>>>,
}

impl Running {
    fn stop(mut self) -> Result<(), DaemonError> {
        self.stop.stop();
        self.thread.take().unwrap().join().unwrap()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(t) = self.thread.take() {
            self.stop.stop();
            let _ = t.join();
        }
    }
}

fn env() -> Env {
    let dir = tempfile::Builder::new()
        .prefix("hub-daemon-")
        .tempdir()
        .unwrap();
    let root = dir.path().to_path_buf();
    let profile_dir = root.join("profile");
    let proj_dir = profile_dir.join("projects").join("C--fixture");
    fs::create_dir_all(profile_dir.join("sessions")).unwrap();
    fs::create_dir_all(&proj_dir).unwrap();
    let data_dir = root.join("data");
    let relay_dir = data_dir.join("relay");
    fs::create_dir_all(&relay_dir).unwrap();
    let db_path = data_dir.join("hub.db");
    let reader = Store::open(&db_path, true).unwrap();
    let clock = Arc::new(AtomicI64::new(wall_ms()));
    Env {
        _root: dir,
        root,
        profile_dir,
        proj_dir,
        run_dir: data_dir.join("run"),
        data_dir,
        relay_dir,
        db_path,
        cli: Arc::new(FakeCli::new(clock.clone())),
        clock,
        watch: FakeWatch::new(true),
        log: Arc::new(MemoryLogger::new()),
        platform: Arc::new(TestPlatform::new(true, Some(OWN_START))),
        reader,
    }
}

impl Env {
    fn now(&self) -> i64 {
        self.clock.load(SeqCst)
    }

    fn advance(&self, ms: i64) {
        self.clock.fetch_add(ms, SeqCst);
    }

    fn deps(&self, o: Over, stop: StopToken) -> DaemonDeps {
        let platform = o.platform.unwrap_or_else(|| self.platform.clone());
        let identify_platform = platform.clone();
        let clock = self.clock.clone();
        DaemonDeps {
            cli: o.cli.unwrap_or_else(|| self.cli.clone()),
            store: o
                .store
                .unwrap_or_else(|| Store::open(&self.db_path, true).unwrap()),
            platform,
            now: Arc::new(move || clock.load(SeqCst)),
            profiles: o.profiles.unwrap_or_else(|| {
                vec![Profile {
                    id: ProfileId::Claude,
                    dir: self.profile_dir.clone(),
                }]
            }),
            data_dir: self.data_dir.clone(),
            relay_dir: self.relay_dir.clone(),
            stop,
            watch: o.watch.unwrap_or_else(|| self.watch.watch_fn()),
            log: o.log.unwrap_or_else(|| self.log.clone()),
            daemon_identity: Box::new(move |l| identify_daemon(l, &*identify_platform, false)),
            on_records: o.on_records,
            expire: o.expire,
            timing: o.timing.unwrap_or_else(test_timing),
        }
    }

    fn start(&self, o: Over) -> Running {
        let stop = StopToken::new();
        let deps = self.deps(o, stop.clone());
        Running {
            stop,
            thread: Some(std::thread::spawn(move || run_daemon(deps))),
        }
    }

    fn entries(&self) -> Vec<LogEntry> {
        self.log.entries()
    }

    fn count(&self, event: &str) -> usize {
        self.entries().iter().filter(|e| e.event == event).count()
    }

    fn find(&self, event: &str) -> Option<LogEntry> {
        self.entries().into_iter().find(|e| e.event == event)
    }

    fn passes(&self) -> usize {
        self.count("pass")
    }

    /// Fires the watch and waits for the liveness pass it starts.
    fn next_pass(&self) {
        let n = self.passes();
        self.watch.fire();
        wait_for(|| self.passes() > n, 4_000);
    }

    /// Waits for the condition and for the startup pass to have finished.
    fn settled(&self, cond: impl Fn() -> bool) {
        wait_for(|| cond() && self.passes() >= 1, 4_000);
    }

    fn session(&self, id: &str) -> Option<SessionView> {
        self.reader
            .list_sessions()
            .unwrap()
            .into_iter()
            .find(|s| s.session_id == id)
    }

    fn history_len(&self, id: &str) -> usize {
        self.reader.history(id, 100_000, None).unwrap().len()
    }

    fn meta(&self, key: &str) -> Option<String> {
        self.reader.get_meta(key).unwrap()
    }

    fn transcript(&self, session_id: &str, lines: &[String]) -> PathBuf {
        write_transcript(&self.proj_dir, session_id, lines)
    }
}

fn wait_for(cond: impl Fn() -> bool, ms: u64) {
    let end = Instant::now() + Duration::from_millis(ms);
    while !cond() {
        assert!(Instant::now() < end, "wait_for timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn field<'a>(e: &'a LogEntry, key: &str) -> Option<&'a LogValue> {
    e.fields.iter().find(|(k, _)| *k == key).map(|(_, v)| v)
}

fn num(e: &LogEntry, key: &str) -> f64 {
    match field(e, key) {
        Some(LogValue::Num(n)) => *n,
        other => panic!("{key}: {other:?}"),
    }
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

// ---- tests (same order as collector.test.ts) ----

#[test]
fn new_live_session_appears_with_parsed_status_mode_and_masked_messages() {
    let e = env();
    e.transcript(
        S1,
        &[
            mode_line("default"),
            user_line("u1", PROMPT, None),
            mode_line("bypassPermissions"),
            assistant_line("a1", &status_reply(), None),
        ],
    );
    e.cli.set_agents(vec![agent(S1, json!({"status": "busy"}))]);
    type Got = Arc<Mutex<Vec<(String, Vec<Classified>)>>>;
    let got: Got = Arc::default();
    let sink = got.clone();
    let _d = e.start(Over {
        on_records: Some(Box::new(move |_store, id, recs| {
            sink.lock().unwrap().push((id.to_string(), recs.to_vec()));
            Ok(())
        })),
        ..Over::default()
    });
    wait_for(|| e.session(S1).is_some_and(|s| s.status.is_some()), 4_000);

    let s = e.session(S1).unwrap();
    assert_eq!(s.name, format!("name-{S1}"));
    assert_eq!(s.state, LiveStatus::Busy);
    assert_eq!(s.profile, "claude");
    assert_eq!(s.ended_at, None);
    assert_eq!(s.permission_mode.as_deref(), Some("bypassPermissions"));
    assert_eq!(
        s.transcript_path,
        path_str(&e.proj_dir.join(format!("{S1}.jsonl")))
    );
    let st = s.status.as_ref().unwrap();
    assert_eq!(st.progress_pct, Some(40));
    assert_eq!(st.phase.as_deref(), Some(PHASE));
    assert_eq!(st.reply_uuid, "a1");
    let latest = s.latest_reply.as_ref().unwrap();
    assert_eq!(
        (latest.reply_uuid.as_str(), latest.has_status),
        ("a1", true)
    );
    assert_eq!(st.waiting[0].item.id.as_deref(), Some("Q1"));
    assert!(st.waiting[0].item.is_new);

    let hist = e.reader.history(S1, 10, None).unwrap();
    let roles: Vec<&str> = hist.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, ["owner", "assistant"]);
    assert_eq!(hist[0].text, PROMPT);
    let got = got.lock().unwrap();
    assert!(got.iter().any(|(id, recs)| {
        id == S1
            && recs
                .iter()
                .any(|r| matches!(r, Classified::OwnerPrompt { .. }))
    }));
}

#[test]
fn a_secret_in_a_status_reply_is_masked_in_stored_status_and_messages() {
    let e = env();
    e.transcript(S1, &[assistant_line("a1", &status_reply(), None)]);
    e.cli.set_agents(vec![agent(S1, json!({}))]);
    let _d = e.start(Over::default());
    wait_for(|| e.session(S1).is_some_and(|s| s.status.is_some()), 4_000);
    let st = e.session(S1).unwrap().status.unwrap();
    let all = format!(
        "{}{}",
        serde_json::to_string(&st).unwrap(),
        serde_json::to_string(&e.reader.history(S1, 10, None).unwrap()).unwrap()
    );
    assert!(!all.contains(&secret()));
    assert!(all.contains("[masked:github]"));
    assert!(st.remaining[0].contains("[masked:github]"));
    assert!(st.waiting[0].item.text.contains("[masked:github]"));
}

#[test]
fn session_missing_from_agents_is_marked_ended_and_reappearing_clears_it() {
    let e = env();
    e.cli.set_agents(vec![agent(S1, json!({}))]);
    let _d = e.start(Over::default());
    e.settled(|| e.session(S1).is_some());
    e.advance(5_000);
    let ended_at = e.now();
    e.cli.set_agents(vec![]);
    e.next_pass();
    assert_eq!(e.session(S1).unwrap().ended_at, Some(ended_at));
    e.cli.set_agents(vec![agent(S1, json!({}))]);
    e.next_pass();
    assert_eq!(e.session(S1).unwrap().ended_at, None);
}

#[test]
fn identical_agents_output_leaves_the_row_and_database_alone_a_change_bumps_updated_at() {
    let e = env();
    e.transcript(S1, &[user_line("u-1", PROMPT, None)]);
    e.cli.set_agents(vec![agent(S1, json!({}))]);
    let _d = e.start(Over::default());
    e.settled(|| e.session(S1).is_some());
    e.next_pass(); // the transcript path is found and stored on a pass; this one has settled
    let reader = Store::open(&e.db_path, true).unwrap();
    let before = e.session(S1).unwrap().updated_at;
    let version = reader.data_version().unwrap();
    e.advance(60_000);
    e.next_pass();
    e.advance(60_000);
    e.next_pass();
    assert_eq!(e.session(S1).unwrap().updated_at, before);
    assert_eq!(reader.data_version().unwrap(), version);
    e.cli.set_agents(vec![agent(S1, json!({"status": "busy"}))]);
    e.next_pass();
    let s = e.session(S1).unwrap();
    assert_eq!(s.state, LiveStatus::Busy);
    assert_eq!(s.updated_at, e.now());
    assert_ne!(reader.data_version().unwrap(), version);
}

#[test]
fn a_new_reply_bumps_updated_at() {
    let e = env();
    let path = e.transcript(S1, &[user_line("u-1", PROMPT, None)]);
    e.cli.set_agents(vec![agent(S1, json!({}))]);
    let _d = e.start(Over::default());
    e.settled(|| e.session(S1).is_some());
    e.next_pass();
    let before = e.session(S1).unwrap().updated_at;
    e.advance(60_000);
    append(
        &path,
        &format!(
            "{}\n",
            assistant_line("a-1", &status_reply(), Some(e.now()))
        ),
    );
    e.next_pass();
    let after = e.session(S1).unwrap().updated_at;
    assert_eq!(after, e.now());
    assert!(after > before);
}

#[test]
fn an_appended_owner_message_moves_updated_at() {
    let e = env();
    let path = e.transcript(S1, &[user_line("u-1", PROMPT, None)]);
    e.cli.set_agents(vec![agent(S1, json!({}))]);
    let _d = e.start(Over::default());
    e.settled(|| e.session(S1).is_some());
    e.next_pass();
    let before = e.session(S1).unwrap().updated_at;
    e.advance(60_000);
    append(
        &path,
        &format!("{}\n", user_line("u-2", PLAIN, Some(e.now()))),
    );
    e.next_pass();
    let after = e.session(S1).unwrap().updated_at;
    assert_eq!(after, e.now());
    assert!(after > before);
}

#[test]
fn own_relay_sessions_are_hidden() {
    let e = env();
    e.cli.set_agents(vec![
        agent(
            R1,
            json!({"name": RELAY_NAME, "cwd": path_str(&e.relay_dir)}),
        ),
        agent(S1, json!({})),
    ]);
    let _d = e.start(Over::default());
    wait_for(|| e.session(S1).is_some(), 4_000);
    assert!(e.session(R1).is_none());
}

#[test]
fn partial_line_written_mid_record_then_completed_gives_one_message_no_duplicate() {
    let e = env();
    let path = e.transcript(S1, &[user_line("u1", PROMPT, None)]);
    e.cli.set_agents(vec![agent(S1, json!({}))]);
    let _d = e.start(Over::default());
    e.settled(|| e.history_len(S1) == 1);
    let line = assistant_line("a1", PARTIAL, None);
    let cut = line.len() / 2;
    append(&path, &line[..cut]);
    e.next_pass();
    assert_eq!(e.history_len(S1), 1);
    append(&path, &format!("{}\n", &line[cut..]));
    e.next_pass();
    e.next_pass();
    let hist = e.reader.history(S1, 10, None).unwrap();
    let uuids: Vec<&str> = hist.iter().map(|m| m.uuid.as_str()).collect();
    assert_eq!(uuids, ["u1", "a1"]);
    assert_eq!(hist[1].text, PARTIAL);
    let latest = e.session(S1).unwrap().latest_reply.unwrap();
    assert_eq!(
        (latest.reply_uuid.as_str(), latest.has_status),
        ("a1", false)
    );
}

#[test]
fn transcript_created_after_the_session_starts_is_found_on_a_later_pass() {
    let e = env();
    e.cli.set_agents(vec![agent(S1, json!({}))]);
    let _d = e.start(Over::default());
    e.settled(|| e.session(S1).is_some());
    assert_eq!(e.session(S1).unwrap().transcript_path, "");
    e.transcript(S1, &[user_line("u1", PROMPT, None)]);
    e.next_pass();
    wait_for(|| e.history_len(S1) == 1, 4_000);
    assert_eq!(
        e.session(S1).unwrap().transcript_path,
        path_str(&e.proj_dir.join(format!("{S1}.jsonl")))
    );
}

#[test]
fn unknown_records_are_counted_in_meta() {
    let e = env();
    e.transcript(
        S1,
        &[
            unknown_line(),
            unrecognized_line(),
            unknown_line(),
            unrecognized_line(),
            unrecognized_line(),
            user_line("u1", PROMPT, None),
        ],
    );
    e.cli.set_agents(vec![agent(S1, json!({}))]);
    let _d = e.start(Over::default());
    e.settled(|| e.history_len(S1) == 1);
    assert_eq!(e.meta("unknown_count").as_deref(), Some("2"));
    assert_eq!(e.meta("unknown_version").as_deref(), Some("9.9.9"));
    assert_eq!(e.meta("unrecognized_count").as_deref(), Some("3"));
    assert_eq!(
        e.meta("unrecognized_types").as_deref(),
        Some(r#"["brand-new-record-type"]"#)
    );
    let pass = e
        .entries()
        .into_iter()
        .find(|x| x.event == "pass" && field(x, "lines") == Some(&LogValue::Num(6.0)))
        .expect("a pass that read 6 lines");
    assert_eq!(num(&pass, "unknown"), 2.0);
    assert_eq!(num(&pass, "unrecognized"), 3.0);
}

#[test]
fn agents_failure_sets_stale_since_backs_off_2_then_4_s_and_clears_on_recovery() {
    let e = env();
    e.cli.set_agents(vec![agent(S1, json!({}))]);
    e.cli.set_fail(true);
    let t0 = e.now();
    let _d = e.start(Over::default());
    let failures = || e.count("agents_failed");
    wait_for(|| failures() == 1 && e.passes() >= 1, 4_000);
    assert_eq!(e.cli.calls(), [t0]);
    assert_eq!(e.meta("stale_since:claude"), Some(t0.to_string()));
    let f = e.find("agents_failed").unwrap();
    assert_eq!(num(&f, "failures"), 1.0);
    assert_eq!(num(&f, "backoffMs"), BACKOFF_MS[0] as f64);

    e.next_pass();
    e.advance(1_999);
    e.next_pass();
    assert_eq!(e.cli.calls().len(), 1);
    e.advance(1);
    e.watch.fire();
    wait_for(|| failures() == 2, 4_000);

    e.advance(3_999);
    e.next_pass();
    assert_eq!(e.cli.calls().len(), 2);
    e.advance(1);
    e.watch.fire();
    wait_for(|| failures() == 3, 4_000);
    assert_eq!(e.cli.calls(), [t0, t0 + 2_000, t0 + 6_000]);
    assert_eq!(e.meta("stale_since:claude"), Some(t0.to_string()));

    e.cli.set_fail(false);
    e.advance(8_000);
    e.watch.fire();
    wait_for(|| e.meta("stale_since:claude").is_none(), 4_000);
    assert_eq!(e.cli.calls(), [t0, t0 + 2_000, t0 + 6_000, t0 + 14_000]);
    assert!(e.session(S1).is_some());
    assert_eq!(e.count("agents_recovered"), 1);
}

#[test]
fn after_a_restart_with_agents_failing_stored_live_sessions_are_still_tailed() {
    let e = env();
    let path = e.transcript(S1, &[user_line("u1", PROMPT, None)]);
    e.reader
        .upsert_session(&SessionRow {
            session_id: S1.into(),
            profile: "claude".into(),
            name: "n".into(),
            cwd: "C:\\fixture".into(),
            pid: 1,
            kind: "interactive".into(),
            status: LiveStatus::Idle,
            started_at: 1,
            updated_at: 1,
            ended_at: None,
            transcript_path: path_str(&path),
            permission_mode: None,
        })
        .unwrap();
    e.cli.set_fail(true);
    let _d = e.start(Over::default());
    e.settled(|| e.history_len(S1) == 1);
    assert_eq!(e.session(S1).unwrap().ended_at, None);
    assert!(e.meta("stale_since:claude").is_some());
}

#[test]
fn agents_failure_keeps_the_profiles_sessions_as_they_are() {
    let e = env();
    e.cli.set_agents(vec![agent(S1, json!({}))]);
    let _d = e.start(Over::default());
    e.settled(|| e.session(S1).is_some());
    e.cli.set_fail(true);
    e.cli.set_agents(vec![]);
    e.next_pass();
    assert_eq!(e.session(S1).unwrap().ended_at, None);
    assert!(e.meta("stale_since:claude").is_some());
}

#[test]
fn second_daemon_is_refused_by_the_lock() {
    let e = env();
    let _d = e.start(Over::default());
    wait_for(|| e.passes() >= 1, 4_000);
    let second = run_daemon(e.deps(
        Over {
            profiles: Some(vec![]),
            ..Over::default()
        },
        StopToken::new(),
    ));
    let err = second.unwrap_err();
    assert!(matches!(err, DaemonError::AlreadyRunning));
    assert_eq!(err.to_string(), "daemon already running");
}

#[test]
fn a_stale_lock_left_by_a_dead_daemon_is_taken_over() {
    let e = env();
    fs::create_dir_all(&e.run_dir).unwrap();
    // Only this process is alive on the test platform: any other pid is a dead daemon.
    let dead = std::process::id().wrapping_add(1).max(2);
    fs::write(e.run_dir.join(LOCK_FILE), dead.to_string()).unwrap();
    let _d = e.start(Over::default());
    wait_for(|| e.passes() >= 1, 4_000);
    assert_eq!(
        fs::read_to_string(e.run_dir.join(LOCK_FILE)).unwrap(),
        format!("{} {OWN_START}", std::process::id())
    );
}

#[test]
fn data_dir_that_is_not_private_fails_before_taking_the_lock() {
    let e = env();
    let platform = Arc::new(TestPlatform::new(false, Some(OWN_START)));
    let err = run_daemon(e.deps(
        Over {
            profiles: Some(vec![]),
            platform: Some(platform.clone()),
            ..Over::default()
        },
        StopToken::new(),
    ))
    .unwrap_err();
    assert!(matches!(err, DaemonError::NotPrivate));
    assert_eq!(err.to_string(), "data dir is not private");
    assert_eq!(platform.calls(), ["is_private_dir"]);
    assert!(!e.run_dir.join(LOCK_FILE).exists());
}

#[test]
fn own_start_time_unavailable_refuses_to_start_and_writes_no_lock() {
    // R13b: a pid-only lock would read as foreign to the TS daemon (it names hubw.exe), which would
    // take it over: two writers on hub.db.
    let e = env();
    let platform = Arc::new(TestPlatform::new(true, None));
    let err = run_daemon(e.deps(
        Over {
            platform: Some(platform.clone()),
            ..Over::default()
        },
        StopToken::new(),
    ))
    .unwrap_err();
    assert!(matches!(err, DaemonError::NoStartTime));
    assert!(!e.run_dir.join(LOCK_FILE).exists());
    assert!(e.cli.calls().is_empty());
}

#[test]
fn stop_returns_and_releases_the_lock() {
    let e = env();
    e.cli.set_agents(vec![agent(S1, json!({}))]);
    let d = e.start(Over::default());
    wait_for(|| e.passes() >= 1, 4_000);
    assert!(e.run_dir.join(LOCK_FILE).exists());
    d.stop().unwrap();
    assert!(!e.run_dir.join(LOCK_FILE).exists());
    assert_eq!(e.count("stop"), 1);
    let start = e.find("start").unwrap();
    assert_eq!(num(&start, "profiles"), 1.0);
    let lock = acquire_lock(
        &e.run_dir,
        LockOptions {
            identify: &|_| unreachable!("no lock is left"),
            started_at: None,
        },
    )
    .unwrap();
    assert!(lock.is_some());
}

fn set_mtime(path: &Path, at_ms: i64) {
    let t = UNIX_EPOCH + Duration::from_millis(at_ms as u64);
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

fn mtime_ms(path: &Path) -> i64 {
    fs::metadata(path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[test]
fn recent_transcript_indexed_at_startup_one_older_than_7_days_or_not_uuid_named_is_not() {
    let e = env();
    let recent = e.transcript(
        E1,
        &[
            mode_line("default"),
            user_line("eu1", ENDED, None),
            assistant_line("ea1", &status_reply(), None),
        ],
    );
    let old = e.transcript(O1, &[user_line("ou1", PLAIN, None)]);
    set_mtime(&old, e.now() - RETENTION_MS as i64 - 86_400_000);
    let nested = e.proj_dir.join("nested");
    fs::create_dir(&nested).unwrap();
    write_transcript(&nested, "deep", &[user_line("du1", PLAIN, None)]);
    fs::write(e.proj_dir.join("notes.txt"), "x").unwrap();
    e.transcript("not-a-session-id", &[user_line("nu1", PLAIN, None)]);

    let _d = e.start(Over::default());
    wait_for(|| e.session(E1).is_some_and(|s| s.status.is_some()), 4_000);
    let s = e.session(E1).unwrap();
    assert_eq!(s.kind, "ended");
    assert_eq!(s.state, LiveStatus::Unknown);
    assert_eq!(s.name, "");
    assert_eq!(s.cwd, "C:\\fixture-cwd");
    assert_eq!(s.transcript_path, path_str(&recent));
    assert_eq!(s.ended_at, Some(mtime_ms(&recent)));
    let uuids: Vec<String> = e
        .reader
        .history(E1, 10, None)
        .unwrap()
        .into_iter()
        .map(|m| m.uuid)
        .collect();
    assert_eq!(uuids, ["eu1", "ea1"]);
    assert!(e.session(O1).is_none());
    assert!(e.session("not-a-session-id").is_none());
    assert!(e.session("deep").is_none());
}

#[test]
fn the_file_logger_writes_daemon_log_with_no_message_text() {
    let e = env();
    e.transcript(
        S1,
        &[
            user_line("u1", PROMPT, None),
            assistant_line("a1", &status_reply(), None),
            assistant_line("a2", PLAIN, None),
        ],
    );
    e.transcript(E1, &[user_line("eu1", ENDED, None)]);
    e.cli.set_agents(vec![agent(S1, json!({}))]);
    let d = e.start(Over {
        log: Some(create_logger(&e.run_dir, LOG_MAX_BYTES, DAEMON_LOG)),
        ..Over::default()
    });
    wait_for(|| e.history_len(S1) == 3 && e.history_len(E1) == 1, 4_000);
    e.cli.set_fail(true);
    e.watch.fire();
    let log_file = e.run_dir.join(DAEMON_LOG);
    wait_for(
        || fs::read_to_string(&log_file).is_ok_and(|t| t.contains("agents_failed")),
        4_000,
    );
    d.stop().unwrap();
    let mut content = String::new();
    for f in fs::read_dir(&e.run_dir).unwrap() {
        let f = f.unwrap();
        if f.file_name().to_string_lossy().starts_with(DAEMON_LOG) {
            content.push_str(&fs::read_to_string(f.path()).unwrap());
        }
    }
    assert!(!content.is_empty());
    let s = secret();
    for t in TEXTS
        .iter()
        .copied()
        .chain([s.as_str(), "Progress", "Waiting on you"])
    {
        assert!(!content.contains(t), "{t}");
    }
}

#[test]
fn invalid_session_ids_from_agents_are_skipped_and_only_counted() {
    let e = env();
    e.cli
        .set_agents(vec![agent("....evil", json!({})), agent(S1, json!({}))]);
    let _d = e.start(Over::default());
    e.settled(|| e.session(S1).is_some());
    let ids: Vec<String> = e
        .reader
        .list_sessions()
        .unwrap()
        .into_iter()
        .map(|s| s.session_id)
        .collect();
    assert_eq!(ids, [S1]);
    let ev = e.find("invalid_session_ids").unwrap();
    assert_eq!(
        ev.fields,
        vec![
            ("profile", LogValue::Str("claude".into())),
            ("count", LogValue::Num(1.0))
        ]
    );
}

#[test]
fn a_transcript_cut_by_the_8_mb_budget_is_fully_indexed_after_a_restart() {
    let e = env();
    let pad = "x".repeat(1000);
    let total = 17_000;
    let lines: Vec<String> = (0..total)
        .map(|i| {
            json!({
                "type": "user", "isSidechain": false, "uuid": format!("big-{i}"),
                "timestamp": "2026-11-13T08:00:00.000Z", "origin": {"kind": "human"},
                "message": {"role": "user", "content": format!("p{i}")}, "pad": pad,
            })
            .to_string()
        })
        .collect();
    let path = e.transcript(S1, &lines);
    let size = fs::metadata(&path).unwrap().len();
    assert!(size > 16 * 1024 * 1024);
    e.cli.set_agents(vec![agent(S1, json!({}))]);
    // No tail tick: only the startup pass reads (one 8 MB budget).
    let slow = Timing {
        tail_poll: Duration::from_secs(3600),
        ..test_timing()
    };
    let first = e.start(Over {
        timing: Some(slow),
        ..Over::default()
    });
    e.settled(|| e.history_len(S1) >= 1);
    first.stop().unwrap();
    let after_first = e.history_len(S1);
    assert!(after_first < total);
    let key = path_str(&path);
    assert!(e.reader.get_tail(&key).unwrap().unwrap().offset < size);

    let _second = e.start(Over::default());
    wait_for(|| e.history_len(S1) == total, 30_000);
    assert_eq!(e.reader.get_tail(&key).unwrap().unwrap().offset, size);
}

#[test]
fn triggers_during_a_pass_give_exactly_one_follow_up_pass() {
    let e = env();
    e.cli.set_agents(vec![agent(S1, json!({}))]);
    let _d = e.start(Over::default());
    e.settled(|| e.session(S1).is_some());
    let p0 = e.passes();
    let c0 = e.cli.calls().len();
    e.cli.hold();
    e.watch.fire();
    wait_for(|| e.cli.calls().len() == c0 + 1, 4_000);
    e.watch.fire();
    e.watch.fire();
    e.watch.fire();
    e.cli.release();
    wait_for(|| e.passes() == p0 + 2, 4_000);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(e.passes(), p0 + 2);
    assert_eq!(e.cli.calls().len(), c0 + 2);
}

#[test]
fn stop_during_an_in_flight_failing_agents_call_arms_no_retry() {
    let e = env();
    let d = e.start(Over::default());
    e.settled(|| true);
    let c0 = e.cli.calls().len();
    e.cli.set_fail(true);
    e.cli.hold();
    e.watch.fire();
    wait_for(|| e.cli.calls().len() == c0 + 1, 4_000);
    d.stop.stop();
    e.cli.release();
    d.stop().unwrap();
    assert_eq!(e.count("agents_failed"), 0);
    assert_eq!(e.meta("stale_since:claude"), None);
    assert_eq!(e.cli.calls().len(), c0 + 1);
}

#[test]
fn a_missing_sessions_folder_is_watched_once_it_exists() {
    let e = env();
    fs::remove_dir_all(e.profile_dir.join("sessions")).unwrap();
    e.cli.set_agents(vec![agent(S1, json!({}))]);
    let _d = e.start(Over::default());
    e.settled(|| e.session(S1).is_some());
    assert_eq!(e.watch.live(), 0);
    assert_eq!(e.count("watch_error"), 1);
    fs::create_dir(e.profile_dir.join("sessions")).unwrap();
    wait_for(|| e.watch.live() == 1, 3_000);
    assert_eq!(e.count("watch_rearmed"), 1);
    e.next_pass();
}

#[test]
fn a_deleted_live_transcript_clears_the_cached_path_and_is_found_again() {
    let e = env();
    let path = e.transcript(S1, &[user_line("u1", PROMPT, None)]);
    e.cli.set_agents(vec![agent(S1, json!({}))]);
    let _d = e.start(Over::default());
    e.settled(|| e.history_len(S1) == 1);
    fs::remove_file(&path).unwrap();
    e.next_pass();
    e.next_pass();
    assert_eq!(e.session(S1).unwrap().transcript_path, "");
    assert_eq!(e.count("transcript_missing"), 1);
    let other = e.profile_dir.join("projects").join("D--moved");
    fs::create_dir(&other).unwrap();
    let moved = write_transcript(
        &other,
        S1,
        &[user_line("u1", PROMPT, None), user_line("u2", PLAIN, None)],
    );
    e.next_pass();
    wait_for(|| e.history_len(S1) == 2, 4_000);
    assert_eq!(e.session(S1).unwrap().transcript_path, path_str(&moved));
}

#[test]
fn purge_hourly_and_vacuum_daily_and_a_busy_database_is_retried_on_the_next_pass() {
    let e = env();
    let store = Store::open(&e.db_path, true).unwrap();
    // A short busy wait, so the held write lock fails the purge quickly (5 s by default).
    store.set_busy_timeout(Duration::from_millis(200)).unwrap();
    let other = rusqlite::Connection::open(&e.db_path).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let _d = e.start(Over {
        store: Some(store),
        ..Over::default()
    });
    wait_for(|| e.count("purge_failed") > 0, 4_000);
    let failed = e.find("purge_failed").unwrap();
    assert_eq!(field(&failed, "busy"), Some(&LogValue::Bool(true)));
    assert_eq!(e.meta("last_purge"), None);
    assert_eq!(e.meta("last_vacuum"), None);
    assert_eq!(e.count("vacuum_failed"), 0);
    other.execute_batch("COMMIT").unwrap();
    drop(other);

    e.next_pass();
    wait_for(|| e.count("purge") == 1 && e.count("vacuum") == 1, 4_000);
    assert_eq!(e.meta("last_purge"), Some(e.now().to_string()));
    assert_eq!(e.meta("last_vacuum"), Some(e.now().to_string()));

    e.advance(3_599_999);
    e.next_pass();
    assert_eq!(e.count("purge"), 1);
    e.advance(1);
    e.next_pass();
    assert_eq!(e.count("purge"), 2);
    assert_eq!(e.count("vacuum"), 1);
    e.advance(86_400_000);
    e.next_pass();
    assert_eq!(e.count("purge"), 3);
    assert_eq!(e.count("vacuum"), 2);
}

#[test]
fn a_purge_failing_with_a_non_busy_error_is_retried_at_most_every_5_minutes() {
    let e = env();
    // An old send makes the purge delete from `sends`, which this trigger refuses.
    e.reader
        .create_send(&SendRow {
            id: None,
            session_id: S1.into(),
            profile: "claude".into(),
            name: "n".into(),
            sha256: "0".repeat(64),
            created_at: 1,
            state: SendState::Delivered,
            detail: None,
        })
        .unwrap();
    rusqlite::Connection::open(&e.db_path)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER test_fail_purge BEFORE DELETE ON sends
             BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END;",
        )
        .unwrap();
    let _d = e.start(Over::default());
    e.settled(|| true);
    assert_eq!(e.count("purge_failed"), 1);
    let failed = e.find("purge_failed").unwrap();
    assert_eq!(field(&failed, "busy"), Some(&LogValue::Bool(false)));
    assert_eq!(e.count("vacuum"), 1);
    e.next_pass();
    e.next_pass();
    assert_eq!(e.count("purge_failed"), 1);
    e.advance(299_999);
    e.next_pass();
    assert_eq!(e.count("purge_failed"), 1);
    e.advance(1);
    e.next_pass();
    assert_eq!(e.count("purge_failed"), 2);
}

#[test]
fn a_watcher_that_keeps_dying_logs_once_until_a_rearmed_one_survives_a_safety_poll() {
    let e = env();
    let w = FakeWatch::new(false);
    w.dying.store(true, SeqCst);
    let _d = e.start(Over {
        watch: Some(w.watch_fn()),
        ..Over::default()
    });
    e.settled(|| true);
    wait_for(|| w.arms.load(SeqCst) >= 4, 3_000);
    assert_eq!(e.count("watch_error"), 1);
    assert_eq!(e.count("watch_rearmed"), 1);

    w.dying.store(false, SeqCst);
    let a = w.arms.load(SeqCst);
    wait_for(|| w.arms.load(SeqCst) == a + 1, 3_000);
    e.advance(SAFETY_POLL_MS as i64);
    std::thread::sleep(Duration::from_millis(250)); // a tick sees the watcher stable for a poll
    assert_eq!(e.count("watch_error"), 1);
    w.kill(None);
    wait_for(
        || e.count("watch_error") == 2 && e.count("watch_rearmed") == 2,
        3_000,
    );
}

#[test]
fn an_error_from_a_replaced_watcher_is_ignored() {
    let e = env();
    let _d = e.start(Over::default());
    e.settled(|| true);
    assert_eq!(e.watch.arms(), 1);
    e.watch.kill(Some(0));
    wait_for(|| e.watch.arms() == 2, 3_000);
    assert_eq!(e.count("watch_error"), 1);
    e.watch.kill(Some(0)); // the first watcher again: it was replaced
    std::thread::sleep(Duration::from_millis(250)); // several ticks
    assert_eq!(e.watch.arms(), 2);
    assert_eq!(e.watch.live(), 1);
    assert_eq!(e.count("watch_error"), 1);
}

#[test]
fn a_last_line_that_never_completes_is_not_reread_while_the_file_is_unchanged() {
    let e = env();
    let path = e.transcript(S1, &[user_line("u1", PROMPT, None)]);
    let line = user_line("u2", PARTIAL, None);
    let cut = line.len() / 2;
    append(&path, &line[..cut]);
    e.cli.set_agents(vec![agent(S1, json!({}))]);
    let _d = e.start(Over::default());
    e.settled(|| e.history_len(S1) == 1);
    e.next_pass();
    e.next_pass();
    let last = e
        .entries()
        .into_iter()
        .rfind(|x| x.event == "pass")
        .unwrap();
    assert_eq!(num(&last, "files"), 0.0);
    append(&path, &format!("{}\n", &line[cut..]));
    e.next_pass();
    wait_for(|| e.history_len(S1) == 2, 4_000);
}

// ---- beyond the TS file ----

#[test]
fn constants_match_ts() {
    assert_eq!(TAIL_POLL_MS, 2_000);
    assert_eq!(BACKOFF_MS, [2_000, 4_000, 8_000, 16_000, 30_000]);
    let t = Timing::default();
    assert_eq!(t.tail_poll, Duration::from_millis(2_000));
    assert_eq!(t.safety_poll, Duration::from_millis(SAFETY_POLL_MS));
    assert_eq!(t.expire_every, Duration::from_millis(30_000));
}

#[test]
fn tails_key_and_transcript_path_are_the_node_path_join_form() {
    // R13c: TS keys `tails` by `path.join(profileDir, "projects", dir, file)`, which normalises the
    // profile dir (on Windows, `/` becomes `\`). The Rust daemon must write the same key, or the
    // two daemons would index one transcript twice.
    let e = env();
    let native = path_str(&e.profile_dir);
    let odd = if cfg!(windows) {
        native.replace('\\', "/")
    } else {
        format!("{}/./profile", path_str(&e.root))
    };
    let expected = e.proj_dir.join(format!("{S1}.jsonl"));
    let expected = path_str(&expected);
    if cfg!(windows) {
        assert!(!expected.contains('/'), "{expected}");
    }
    e.transcript(S1, &[user_line("u1", PROMPT, None)]);
    e.transcript(E1, &[user_line("eu1", ENDED, None)]);
    e.cli.set_agents(vec![agent(S1, json!({}))]);
    let _d = e.start(Over {
        profiles: Some(vec![Profile {
            id: ProfileId::Claude,
            dir: PathBuf::from(&odd),
        }]),
        ..Over::default()
    });
    e.settled(|| e.history_len(S1) == 1 && e.history_len(E1) == 1);
    assert!(e.reader.get_tail(&expected).unwrap().is_some());
    assert_eq!(e.session(S1).unwrap().transcript_path, expected);
    let ended = path_str(&e.proj_dir.join(format!("{E1}.jsonl")));
    assert!(e.reader.get_tail(&ended).unwrap().is_some());
    assert_eq!(e.session(E1).unwrap().transcript_path, ended);
}

#[test]
fn an_on_records_error_is_logged_as_on_records_failed_and_counted() {
    let e = env();
    e.transcript(S1, &[user_line("u1", PROMPT, None)]);
    e.cli.set_agents(vec![agent(S1, json!({}))]);
    let _d = e.start(Over {
        on_records: Some(Box::new(|_store, _id, _recs| {
            Err(hub_core::store::StoreError::WalUnavailable)
        })),
        ..Over::default()
    });
    e.settled(|| e.history_len(S1) == 1);
    let f = e.find("on_records_failed").unwrap();
    assert_eq!(f.level, "warn");
    assert_eq!(field(&f, "session"), Some(&LogValue::Str(S1.into())));
    assert_eq!(
        field(&f, "msg"),
        Some(&LogValue::Str("sqlite WAL mode unavailable".into()))
    );
    let pass = e
        .entries()
        .into_iter()
        .find(|x| x.event == "pass" && num(x, "files") >= 1.0)
        .unwrap();
    assert_eq!(num(&pass, "errors"), 1.0);
}

#[test]
fn a_panicking_on_records_is_logged_without_its_payload_and_counted() {
    let e = env();
    e.transcript(S1, &[user_line("u1", PROMPT, None)]);
    e.cli.set_agents(vec![agent(S1, json!({}))]);
    let _d = e.start(Over {
        on_records: Some(Box::new(|_store, _id, _recs| panic!("{PROMPT}"))),
        ..Over::default()
    });
    e.settled(|| e.history_len(S1) == 1);
    let f = e.find("on_records_failed").unwrap();
    assert!(!format!("{:?}", f.fields).contains(PROMPT));
    let pass = e
        .entries()
        .into_iter()
        .find(|x| x.event == "pass" && num(x, "files") >= 1.0)
        .unwrap();
    assert_eq!(num(&pass, "errors"), 1.0);
}

#[test]
fn the_expire_task_runs_on_the_collector_thread_every_expire_every() {
    let e = env();
    let runs = Arc::new(AtomicUsize::new(0));
    let seen = runs.clone();
    let _d = e.start(Over {
        expire: Some(Box::new(move |store: &Store| {
            store.get_meta("x").unwrap();
            seen.fetch_add(1, SeqCst);
        })),
        timing: Some(Timing {
            expire_every: Duration::from_millis(30),
            ..test_timing()
        }),
        ..Over::default()
    });
    wait_for(|| runs.load(SeqCst) >= 3, 3_000);
}

#[test]
fn a_failed_agents_call_is_retried_by_its_backoff_timer_without_a_trigger() {
    let e = env();
    e.cli.set_fail(true);
    let _d = e.start(Over::default());
    wait_for(|| e.count("agents_failed") == 1, 4_000);
    // The retry is real time (BACKOFF_MS[0] = 2 s); the manual clock moves past the backoff.
    e.advance(BACKOFF_MS[0] as i64);
    wait_for(|| e.cli.calls().len() == 2, 2_900);
}

#[test]
fn a_stop_marker_is_not_the_collectors_business() {
    // run_daemon leaves run/daemon.stopped alone; daemon_run removes it (tests/daemon_run.rs).
    let e = env();
    fs::create_dir_all(&e.run_dir).unwrap();
    fs::write(e.run_dir.join(DAEMON_STOPPED_FILE), "").unwrap();
    let d = e.start(Over::default());
    wait_for(|| e.passes() >= 1, 4_000);
    d.stop().unwrap();
    assert!(e.run_dir.join(DAEMON_STOPPED_FILE).exists());
}

#[test]
fn an_unreadable_session_row_is_skipped_and_counted_in_the_log_without_its_values() {
    let e = env();
    let path = e.transcript(S1, &[user_line("u1", PROMPT, None)]);
    for (id, name) in [(S1, "good"), ("bad-row", "bad")] {
        e.reader
            .upsert_session(&SessionRow {
                session_id: id.into(),
                profile: "claude".into(),
                name: name.into(),
                cwd: "C:\\fixture".into(),
                pid: 1,
                kind: "interactive".into(),
                status: LiveStatus::Idle,
                started_at: 1,
                updated_at: 1,
                ended_at: None,
                transcript_path: path_str(&path),
                permission_mode: None,
            })
            .unwrap();
    }
    rusqlite::Connection::open(&e.db_path)
        .unwrap()
        .execute_batch(
            r#"INSERT INTO status VALUES ('bad-row', NULL, NULL, '[]', '["leaked-text-123"]',
               '[]', 'r', 't');"#,
        )
        .unwrap();
    e.cli.set_fail(true);
    let _d = e.start(Over::default());
    // The good row still loads and is tailed.
    e.settled(|| e.history_len(S1) == 1);
    let w = e
        .find("sessions_unreadable")
        .expect("sessions_unreadable logged");
    assert_eq!(num(&w, "count"), 1.0);
    assert_eq!(w.fields.len(), 1, "{:?}", w.fields);
    let all = format!("{:?}", e.entries());
    assert!(!all.contains("leaked-text-123"), "{all}");
    // Logged once per change, not on every reload.
    e.advance(5_000);
    e.next_pass();
    assert_eq!(e.count("sessions_unreadable"), 1);
}
