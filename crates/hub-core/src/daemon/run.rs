//! `hub daemon run`: the collector in this process until Ctrl-C or SIGTERM (on Windows also
//! Ctrl-Break and closing the console). Port of `src/cli/daemon-run.ts`.

use std::fs;
use std::io;
use std::panic::{self, AssertUnwindSafe, PanicHookInfo};
use std::path::Path;
use std::sync::Arc;

use crate::config::{Config, DAEMON_STOPPED_FILE};
use crate::daemon::collector::{
    DaemonDeps, DaemonError, RecordsHook, StoreTask, Timing, WatchFn, run_daemon,
};
use crate::daemon::lock::{DaemonIdentity, LOCK_FILE, LockInfo, parse_lock};
use crate::daemon::log::{DAEMON_LOG, LOG_MAX_BYTES, LogValue, Logger, create_logger, err_fields};
use crate::discovery::{ClaudeCli, StopToken};
use crate::platform::Platform;
use crate::send::{delivery_checker, expire_sends};
use crate::store::Store;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    /// Stopped by a signal (or after a panic, which is logged first).
    Stopped,
    /// Another daemon holds the lock (or a pid that cannot be verified does).
    AlreadyRunning,
    /// Could not start or failed; the text names our own error, never message text.
    Failed(String),
}

/// Runs the collector (the real one is [`run_daemon`]).
pub type RunDaemonFn = Box<dyn FnOnce(DaemonDeps) -> Result<(), DaemonError> + Send>;

pub struct DaemonRunDeps {
    pub config: Config,
    pub platform: Arc<dyn Platform>,
    pub cli: Arc<dyn ClaudeCli>,
    pub now: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// Classifies the process daemon.lock names (`identify_daemon` in the binary).
    pub daemon_identity: Arc<dyn Fn(&LockInfo) -> DaemonIdentity + Send + Sync>,
    /// The sessions folder watch (`collector::real_watch` in the binary).
    pub watch: WatchFn,
    /// Replaces [`run_daemon`] (tests); None runs the real collector.
    pub run_daemon: Option<RunDaemonFn>,
}

/// True unless the lock is missing, garbage, or names a "foreign" pid (dead or another program).
fn lock_held(run_dir: &Path, identify: &dyn Fn(&LockInfo) -> DaemonIdentity) -> bool {
    let Ok(bytes) = fs::read(run_dir.join(LOCK_FILE)) else {
        return false;
    };
    parse_lock(&String::from_utf8_lossy(&bytes))
        .is_some_and(|lock| identify(&lock) != DaemonIdentity::Foreign)
}

type Hook = Box<dyn Fn(&PanicHookInfo<'_>) + Sync + Send + 'static>;

/// While alive, every panic is logged as a fixed `panic` event with its location only. The
/// payload is never logged (nor printed): it could quote record text. Dropping it puts the
/// previous hook back.
struct PanicLog {
    prev: Option<Hook>,
}

impl PanicLog {
    fn install(log: Arc<dyn Logger>) -> PanicLog {
        let prev = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let at = info
                .location()
                .map_or_else(String::new, |l| format!("{}:{}", l.file(), l.line()));
            log.error("panic", vec![("at", LogValue::Str(at))]);
        }));
        PanicLog { prev: Some(prev) }
    }
}

impl Drop for PanicLog {
    fn drop(&mut self) {
        // set_hook panics on a panicking thread; the process is going down then anyway.
        if let Some(prev) = self.prev.take()
            && !std::thread::panicking()
        {
            panic::set_hook(prev);
        }
    }
}

fn failed(e: &io::Error) -> RunOutcome {
    RunOutcome::Failed(e.to_string())
}

/// `hub daemon run`. A lock held by our daemon or by a pid that cannot be verified, seen before or
/// during start, means `AlreadyRunning`; only a lock naming a dead pid or another program is stale.
/// Makes the data dir private, creates `run` and `relay`, removes `run/daemon.stopped` (a running
/// daemon ends an earlier deliberate stop), opens the store, then runs the collector with the
/// delivery checker as its records hook and the send expiry every `EXPIRE_EVERY_MS`, until a stop
/// signal. A panic is logged by the panic hook (location only) and the daemon stops cleanly: the
/// collector unwinds, the store closes and the lock is released.
pub fn daemon_run(deps: DaemonRunDeps) -> RunOutcome {
    let config = &deps.config;
    let run_dir = config.run_dir();
    let relay_dir = config.relay_dir();
    if lock_held(&run_dir, &*deps.daemon_identity) {
        return RunOutcome::AlreadyRunning;
    }
    if let Err(e) = deps.platform.ensure_private_dir(&config.data_dir) {
        return failed(&e);
    }
    for dir in [&run_dir, &relay_dir] {
        if let Err(e) = fs::create_dir_all(dir) {
            return failed(&e);
        }
    }
    match fs::remove_file(run_dir.join(DAEMON_STOPPED_FILE)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return failed(&e),
        _ => {}
    }
    // The logger comes first, so a store that cannot open still leaves a line in daemon.log.
    let log = create_logger(&run_dir, LOG_MAX_BYTES, DAEMON_LOG);
    let store = match Store::open(&config.db_path(), true) {
        Ok(s) => s,
        Err(e) => {
            log.error("store_open_failed", err_fields(&e));
            return RunOutcome::Failed(e.to_string());
        }
    };
    let stop = StopToken::new();
    let signals = match deps.platform.stop_on_signals(stop.clone()) {
        Ok(g) => Some(g),
        Err(e) => {
            // Still stoppable by `hub daemon stop` (a kill), only not cleanly by Ctrl-C.
            log.warn("signals_failed", err_fields(&e));
            None
        }
    };
    let panics = PanicLog::install(log.clone());

    let (expire_now, expire_log) = (deps.now.clone(), log.clone());
    let expire: StoreTask = Box::new(move |store| match expire_sends(store, expire_now()) {
        Ok(0) => {}
        Ok(n) => expire_log.info("sends_expired", vec![("count", LogValue::Num(n as f64))]),
        Err(e) => expire_log.warn("expire_failed", err_fields(&e)),
    });
    let mut check = delivery_checker(deps.now.clone(), log.clone());
    let on_records: RecordsHook = Box::new(move |store, id, records| check(store, id, records));
    let identify = deps.daemon_identity.clone();
    let daemon = DaemonDeps {
        cli: deps.cli,
        store,
        platform: deps.platform.clone(),
        now: deps.now,
        profiles: config.profiles.clone(),
        data_dir: config.data_dir.clone(),
        relay_dir,
        stop: stop.clone(),
        watch: deps.watch,
        log: log.clone(),
        daemon_identity: Box::new(move |lock| identify(lock)),
        on_records: Some(on_records),
        expire: Some(expire),
        timing: Timing::default(),
    };
    let run = deps.run_daemon.unwrap_or_else(|| Box::new(run_daemon));
    let res = panic::catch_unwind(AssertUnwindSafe(move || run(daemon)));
    stop.stop();
    drop(panics);
    drop(signals);
    match res {
        Ok(Ok(())) => RunOutcome::Stopped,
        Ok(Err(DaemonError::AlreadyRunning)) => RunOutcome::AlreadyRunning,
        Ok(Err(e)) => {
            log.error("daemon_failed", err_fields(&e));
            RunOutcome::Failed(e.to_string())
        }
        // Logged by the hook; the store is closed and the lock released by the unwind.
        Err(_) => RunOutcome::Stopped,
    }
}
