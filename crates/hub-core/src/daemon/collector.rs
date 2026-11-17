//! The collector loop: refreshes the live session list from `claude agents`, scans recent
//! transcripts, tails them into the index, purges and vacuums, and follows the sessions folder
//! watch. Port of `src/daemon/collector.ts`.
//!
//! Threads, not async: the loop runs on the thread that calls [`run_daemon`]. Watcher callbacks
//! (and a small thread that waits on the stop token) only send messages on a channel; the loop
//! waits on that channel until the next tick, retry or expiry is due. Passes run one at a time on
//! the loop, so they are single-flight by construction: triggers that arrive during a pass queue
//! up and are merged into exactly one follow-up pass.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::ErrorCode;

use crate::config::{EXPIRE_EVERY_MS, Profile, RETENTION_MS, SAFETY_POLL_MS};
use crate::daemon::ingest_pass::{IngestCtx, ingest_file};
use crate::daemon::lock::{DaemonIdentity, LockInfo, LockOptions, acquire_lock};
use crate::daemon::log::{LogFields, LogValue, Logger, err_fields};
use crate::daemon::transcripts::{first_cwd, is_session_id, list_transcripts, transcript_index};
use crate::discovery::{
    ClaudeCli, LiveSession, StopToken, WatchError, WatchHandle, is_own_relay, watch_sessions_with,
};
use crate::ingest::records::Classified;
use crate::platform::Platform;
use crate::store::{LiveStatus, SessionRow, SessionView, Store, StoreError};
use crate::text::is_js_space;

pub const TAIL_POLL_MS: u64 = 2_000;
pub const BACKOFF_MS: [u64; 5] = [2_000, 4_000, 8_000, 16_000, 30_000];
const PURGE_EVERY_MS: i64 = 3_600_000;
const VACUUM_EVERY_MS: i64 = 86_400_000;
/// A purge or vacuum that failed with a non-busy error is retried at most this often.
const MAINT_RETRY_MS: i64 = 300_000;
const RETENTION: f64 = RETENTION_MS as f64;
const SAFETY_POLL: i64 = SAFETY_POLL_MS as i64;
/// How long the stop-forwarding thread sleeps between checks when nothing stops it.
const STOP_WAIT: Duration = Duration::from_secs(3600);

/// Starts a watch of `<profile>/sessions` (the real one is [`real_watch`]): `on_change` on a
/// change, `on_error` when the watcher dies. An `Err` means it could not start.
pub type WatchFn = Box<
    dyn Fn(
            &Path,
            Box<dyn Fn() + Send>,
            Box<dyn Fn(String) + Send + Sync>,
        ) -> Result<WatchHandle, WatchError>
        + Send,
>;

/// The per-batch records hook (the delivery checker). An error is logged as `on_records_failed`
/// and counted, as TS does with a throw.
pub type RecordsHook = Box<dyn FnMut(&Store, &str, &[Classified]) -> Result<(), StoreError> + Send>;

/// Work the collector thread runs every [`Timing::expire_every`] (the daemon's send expiry). It
/// runs between passes, on the loop that owns the store.
pub type StoreTask = Box<dyn FnMut(&Store) + Send>;

/// The loop's real-time intervals. Production uses [`Timing::default`]; tests shorten them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// Tail tick: re-arms dead watchers and tails live transcripts ([`TAIL_POLL_MS`]).
    pub tail_poll: Duration,
    /// Safety poll: a full liveness and scan pass ([`SAFETY_POLL_MS`]).
    pub safety_poll: Duration,
    /// How often the [`DaemonDeps::expire`] task runs ([`EXPIRE_EVERY_MS`]).
    pub expire_every: Duration,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing {
            tail_poll: Duration::from_millis(TAIL_POLL_MS),
            safety_poll: Duration::from_millis(SAFETY_POLL_MS),
            expire_every: Duration::from_millis(EXPIRE_EVERY_MS),
        }
    }
}

pub struct DaemonDeps {
    pub cli: Arc<dyn ClaudeCli>,
    pub store: Store,
    pub platform: Arc<dyn Platform>,
    /// Clock, epoch ms.
    pub now: Arc<dyn Fn() -> i64 + Send + Sync>,
    pub profiles: Vec<Profile>,
    pub data_dir: PathBuf,
    pub relay_dir: PathBuf,
    /// [`run_daemon`] returns once this is stopped and the in-flight pass has finished.
    pub stop: StopToken,
    pub watch: WatchFn,
    pub log: Arc<dyn Logger>,
    /// Classifies the process an existing daemon.lock names (see `acquire_lock`).
    pub daemon_identity: Box<dyn Fn(&LockInfo) -> DaemonIdentity + Send>,
    pub on_records: Option<RecordsHook>,
    pub expire: Option<StoreTask>,
    pub timing: Timing,
}

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("daemon already running")]
    AlreadyRunning,
    #[error("data dir is not private")]
    NotPrivate,
    /// Without it the lock would hold the pid only, which the TS daemon reads as foreign when it
    /// names `hubw.exe`: it would take the lock over and two daemons would write hub.db.
    #[error("own process start time is unavailable")]
    NoStartTime,
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// The real sessions folder watch ([`watch_sessions_with`]).
pub fn real_watch() -> WatchFn {
    Box::new(watch_sessions_with)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Request {
    liveness: bool,
    scan: bool,
}

const TAIL: Request = Request {
    liveness: false,
    scan: false,
};
const CHANGE: Request = Request {
    liveness: true,
    scan: false,
};
const FULL: Request = Request {
    liveness: true,
    scan: true,
};

fn merge(req: &mut Option<Request>, r: Request) {
    *req = Some(match *req {
        Some(q) => Request {
            liveness: q.liveness || r.liveness,
            scan: q.scan || r.scan,
        },
        None => r,
    });
}

enum Msg {
    Changed,
    /// A watcher of profile `profile` (generation `generation`) died.
    WatchFailed {
        profile: usize,
        generation: u64,
        msg: String,
    },
    Stop,
}

#[derive(Default)]
struct ProfileState {
    failures: usize,
    /// now() before which `claude agents` is not called again.
    next_at: i64,
    /// Real time of the retry pass after a failure.
    retry: Option<Instant>,
}

struct Watcher {
    handle: Option<WatchHandle>,
    generation: u64,
    /// False after an error: the watcher is dead and is re-armed once the folder exists.
    armed: bool,
    /// now() of the last re-arm; cleared once that watcher survived one SAFETY_POLL_MS.
    rearmed_at: Option<i64>,
    /// watch_error / watch_rearmed already logged. Stays set until a re-armed watcher has
    /// survived one SAFETY_POLL_MS, so a watcher that keeps dying cannot log every tick.
    error_logged: bool,
    rearm_logged: bool,
}

#[derive(Debug, Default, Clone, Copy)]
struct Counters {
    files: usize,
    lines: usize,
    messages: usize,
    unknown: usize,
    unrecognized: usize,
    errors: usize,
}

/// An error message as an error, for [`err_fields`].
#[derive(Debug)]
struct Message(String);

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for Message {}

fn num(n: usize) -> LogValue {
    LogValue::Num(n as f64)
}

fn text(s: &str) -> LogValue {
    LogValue::Str(s.to_string())
}

/// `{ key: value, ...errFields(e) }`.
fn with_err(mut f: LogFields, e: &dyn Error) -> LogFields {
    f.extend(err_fields(e));
    f
}

fn to_row(v: SessionView) -> SessionRow {
    SessionRow {
        session_id: v.session_id,
        profile: v.profile,
        name: v.name,
        cwd: v.cwd,
        pid: v.pid,
        kind: v.kind,
        status: v.state,
        started_at: v.started_at,
        updated_at: v.updated_at,
        ended_at: v.ended_at,
        transcript_path: v.transcript_path,
        permission_mode: v.permission_mode,
    }
}

/// Every field but `updated_at` is equal (a new field takes part without being listed).
fn same_session_data(a: &SessionRow, b: &SessionRow) -> bool {
    *a == SessionRow {
        updated_at: a.updated_at,
        ..b.clone()
    }
}

fn is_busy(e: &StoreError) -> bool {
    matches!(e, StoreError::Sqlite(rusqlite::Error::SqliteFailure(f, _))
        if f.code == ErrorCode::DatabaseBusy)
}

/// Epoch ms with the fraction (JS `mtimeMs`), 0 before the epoch.
fn epoch_ms(t: SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}

/// `Number(meta ?? "0") || 0` for a stored time.
fn meta_time(v: Option<String>) -> i64 {
    let v = v.unwrap_or_default();
    let t = v.trim_matches(is_js_space);
    if t.is_empty() {
        return 0;
    }
    t.parse::<f64>()
        .ok()
        .filter(|n| n.is_finite())
        .map_or(0, |n| n as i64)
}

fn path_key(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Sets the stop token when dropped, so helper threads end however the loop ends.
struct StopOnDrop(StopToken);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.stop();
    }
}

struct Collector {
    cli: Arc<dyn ClaudeCli>,
    store: Store,
    now: Arc<dyn Fn() -> i64 + Send + Sync>,
    profiles: Vec<Profile>,
    relay_dir: PathBuf,
    stop: StopToken,
    watch: WatchFn,
    log: Arc<dyn Logger>,
    on_records: Option<RecordsHook>,
    expire: Option<StoreTask>,
    timing: Timing,

    sessions: RefCell<HashMap<String, SessionRow>>,
    /// Profile id -> live session ids, in the order `claude agents` listed them.
    live_by_profile: Vec<(String, Vec<String>)>,
    profile_state: Vec<ProfileState>,
    /// Recent transcripts from the last scan: (path, sessionId).
    recent: Vec<(PathBuf, String)>,
    watchers: Vec<Option<Watcher>>,
    next_generation: u64,
    /// path -> "size:mtime" of a file whose unread rest is only an incomplete last line.
    partial_at: HashMap<String, String>,
    /// now() before which a purge / vacuum that failed with a non-busy error is not retried.
    purge_retry_at: i64,
    vacuum_retry_at: i64,
    /// Sessions whose cached transcript vanished and was already logged.
    missing_logged: HashSet<String>,
    /// The unreadable session row count last seen by `load_sessions`.
    unreadable_logged: Cell<usize>,
    counters: Counters,
    tx: Sender<Msg>,
}

impl Collector {
    fn new(d: DaemonDeps, tx: Sender<Msg>) -> Result<Collector, StoreError> {
        let n = d.profiles.len();
        let mut c = Collector {
            cli: d.cli,
            store: d.store,
            now: d.now,
            profiles: d.profiles,
            relay_dir: d.relay_dir,
            stop: d.stop,
            watch: d.watch,
            log: d.log,
            on_records: d.on_records,
            expire: d.expire,
            timing: d.timing,
            sessions: RefCell::default(),
            live_by_profile: Vec::new(),
            profile_state: (0..n).map(|_| ProfileState::default()).collect(),
            recent: Vec::new(),
            watchers: (0..n).map(|_| None).collect(),
            next_generation: 0,
            partial_at: HashMap::new(),
            purge_retry_at: 0,
            vacuum_retry_at: 0,
            missing_logged: HashSet::new(),
            unreadable_logged: Cell::new(0),
            counters: Counters::default(),
            tx,
        };
        let rows = c.load_sessions()?;
        // Until a profile's first successful agents call, its last known live list is the store's.
        for v in &rows {
            if v.ended_at.is_none() {
                c.live_set(&v.profile).push(v.session_id.clone());
            }
        }
        c.set_sessions(rows);
        Ok(c)
    }

    fn set_sessions(&self, rows: Vec<SessionView>) {
        let mut s = self.sessions.borrow_mut();
        s.clear();
        for v in rows {
            s.insert(v.session_id.clone(), to_row(v));
        }
    }

    fn reload_sessions(&self) -> Result<(), StoreError> {
        let rows = self.load_sessions()?;
        self.set_sessions(rows);
        Ok(())
    }

    /// The readable session rows. Rows that cannot be decoded are skipped by the store; their count
    /// (never their values) is logged when it changes.
    fn load_sessions(&self) -> Result<Vec<SessionView>, StoreError> {
        let list = self.store.list_sessions_counted()?;
        if self.unreadable_logged.replace(list.unreadable) != list.unreadable && list.unreadable > 0
        {
            self.log
                .warn("sessions_unreadable", vec![("count", num(list.unreadable))]);
        }
        Ok(list.sessions)
    }

    fn live_set(&mut self, profile: &str) -> &mut Vec<String> {
        let i = match self.live_by_profile.iter().position(|(p, _)| p == profile) {
            Some(i) => i,
            None => {
                self.live_by_profile.push((profile.to_string(), Vec::new()));
                self.live_by_profile.len() - 1
            }
        };
        &mut self.live_by_profile[i].1
    }

    fn now(&self) -> i64 {
        (self.now)()
    }

    fn stopped(&self) -> bool {
        self.stop.is_stopped()
    }

    fn row(&self, id: &str) -> Option<SessionRow> {
        self.sessions.borrow().get(id).cloned()
    }

    fn put_row(&self, row: SessionRow) {
        self.sessions
            .borrow_mut()
            .insert(row.session_id.clone(), row);
    }

    fn run(&mut self, rx: &Receiver<Msg>) {
        let count = self.sessions.borrow().len();
        self.log.info(
            "start",
            vec![
                ("profiles", num(self.profiles.len())),
                ("sessions", num(count)),
            ],
        );
        for i in 0..self.profiles.len() {
            self.arm(i, None);
        }
        let mut req = Some(FULL);
        let start = Instant::now();
        let mut next_tail = start + self.timing.tail_poll;
        let mut next_safety = start + self.timing.safety_poll;
        let mut next_expire = start + self.timing.expire_every;
        loop {
            if self.stopped() {
                break;
            }
            if let Some(r) = req.take() {
                self.pass(r);
            }
            // Wait for a trigger, or until the next timer is due.
            let mut due = next_tail.min(next_safety);
            if self.expire.is_some() {
                due = due.min(next_expire);
            }
            for st in &self.profile_state {
                if let Some(at) = st.retry {
                    due = due.min(at);
                }
            }
            let first = match rx.recv_timeout(due.saturating_duration_since(Instant::now())) {
                Ok(m) => Some(m),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            // Everything that arrived (also during the pass) merges into one request.
            for m in first.into_iter().chain(rx.try_iter()) {
                match m {
                    Msg::Changed => merge(&mut req, CHANGE),
                    Msg::WatchFailed {
                        profile,
                        generation,
                        msg,
                    } => self.watch_failed(profile, generation, &msg),
                    Msg::Stop => {}
                }
            }
            if self.stopped() {
                break;
            }
            let now = Instant::now();
            if now >= next_tail {
                // The re-arm check costs one stat per dead watcher only, so it runs every tick.
                self.rearm();
                merge(&mut req, TAIL);
                next_tail = now + self.timing.tail_poll;
            }
            if now >= next_safety {
                self.rearm();
                merge(&mut req, FULL);
                next_safety = now + self.timing.safety_poll;
            }
            for st in &mut self.profile_state {
                if st.retry.is_some_and(|at| now >= at) {
                    st.retry = None;
                    merge(&mut req, CHANGE);
                }
            }
            if now >= next_expire {
                next_expire = now + self.timing.expire_every;
                if let Some(task) = self.expire.as_mut() {
                    task(&self.store);
                }
            }
        }
        // Stopping: the in-flight pass has finished (it checks the token between steps).
        for w in &mut self.watchers {
            w.take();
        }
        self.log.info("stop", Vec::new());
    }

    /// Arms (or re-arms) a profile's watcher; log flags carry over from the previous one.
    fn arm(&mut self, i: usize, rearmed_at: Option<i64>) {
        let prev = self.watchers[i].take();
        let (error_logged, rearm_logged) = prev
            .as_ref()
            .map_or((false, false), |w| (w.error_logged, w.rearm_logged));
        drop(prev); // stops the old watcher
        let generation = self.next_generation;
        self.next_generation += 1;
        let changed = self.tx.clone();
        let failed = self.tx.clone();
        let on_change: Box<dyn Fn() + Send> = Box::new(move || {
            let _ = changed.send(Msg::Changed);
        });
        let on_error: Box<dyn Fn(String) + Send + Sync> = Box::new(move |msg| {
            let _ = failed.send(Msg::WatchFailed {
                profile: i,
                generation,
                msg,
            });
        });
        let mut w = Watcher {
            handle: None,
            generation,
            armed: true,
            rearmed_at,
            error_logged,
            rearm_logged,
        };
        match (self.watch)(&self.profiles[i].dir, on_change, on_error) {
            Ok(h) => w.handle = Some(h),
            Err(e) => {
                w.armed = false;
                if !w.error_logged {
                    w.error_logged = true;
                    self.log.warn(
                        "watch_error",
                        with_err(vec![("profile", text(self.profiles[i].id.as_str()))], &e),
                    );
                }
            }
        }
        self.watchers[i] = Some(w);
    }

    /// A watcher died. Errors from a watcher that was replaced since are ignored.
    fn watch_failed(&mut self, i: usize, generation: u64, msg: &str) {
        let Some(w) = self.watchers.get_mut(i).and_then(Option::as_mut) else {
            return;
        };
        if w.generation != generation {
            return;
        }
        w.armed = false;
        if w.error_logged {
            return;
        }
        w.error_logged = true;
        let profile = text(self.profiles[i].id.as_str());
        self.log.warn(
            "watch_error",
            with_err(vec![("profile", profile)], &Message(msg.to_string())),
        );
    }

    /// Every tick: re-arms dead watchers whose sessions folder now exists, and re-enables logging
    /// for a re-armed watcher that stayed up for one SAFETY_POLL_MS.
    fn rearm(&mut self) {
        if self.stopped() {
            return;
        }
        let t = self.now();
        for i in 0..self.profiles.len() {
            let Some(w) = self.watchers[i].as_mut() else {
                continue;
            };
            if w.armed {
                if w.rearmed_at.is_some_and(|at| t - at >= SAFETY_POLL) {
                    w.rearmed_at = None;
                    w.error_logged = false;
                    w.rearm_logged = false;
                }
                continue;
            }
            if !self.profiles[i].dir.join("sessions").is_dir() {
                continue;
            }
            self.arm(i, Some(t));
            let profile = text(self.profiles[i].id.as_str());
            if let Some(fresh) = self.watchers[i].as_mut()
                && fresh.armed
                && !fresh.rearm_logged
            {
                fresh.rearm_logged = true;
                self.log.info("watch_rearmed", vec![("profile", profile)]);
            }
        }
    }

    fn pass(&mut self, r: Request) {
        let t0 = Instant::now();
        let mut live = 0;
        let mut ended = 0;
        if let Err(e) = self.pass_steps(r, &mut live, &mut ended) {
            self.counters.errors += 1;
            self.log.error("pass_failed", err_fields(e.as_ref()));
        }
        if r.liveness || r.scan {
            let c = std::mem::take(&mut self.counters);
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            self.log.info(
                "pass",
                vec![
                    ("ms", LogValue::Num(ms.round())),
                    ("live", num(live)),
                    ("ended", num(ended)),
                    ("files", num(c.files)),
                    ("lines", num(c.lines)),
                    ("messages", num(c.messages)),
                    ("unknown", num(c.unknown)),
                    ("unrecognized", num(c.unrecognized)),
                    ("errors", num(c.errors)),
                ],
            );
        }
    }

    fn pass_steps(
        &mut self,
        r: Request,
        live: &mut usize,
        ended: &mut usize,
    ) -> Result<(), Box<dyn Error>> {
        if r.liveness {
            (*live, *ended) = self.liveness();
        }
        if r.scan {
            self.scan_recent()?;
        }
        for id in self.live_ids() {
            if self.stopped() {
                break;
            }
            if let Some(row) = self.row(&id)
                && !row.transcript_path.is_empty()
            {
                self.tail_if_changed(&id, Path::new(&row.transcript_path));
            }
        }
        if r.scan {
            for (path, id) in self.recent.clone() {
                if self.stopped() {
                    break;
                }
                let key = path_key(&path);
                if self
                    .row(&id)
                    .is_some_and(|row| row.ended_at.is_some() && row.transcript_path == key)
                {
                    self.tail_if_changed(&id, &path);
                }
            }
        }
        // Gated by meta last-run times; a busy database is retried on the next such pass.
        if r.liveness || r.scan {
            self.maintenance()?;
        }
        Ok(())
    }

    fn live_ids(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        self.live_by_profile
            .iter()
            .flat_map(|(_, ids)| ids.iter())
            .filter(|id| seen.insert(id.as_str()))
            .cloned()
            .collect()
    }

    fn liveness(&mut self) -> (usize, usize) {
        let mut live = 0;
        let mut ended = 0;
        for i in 0..self.profiles.len() {
            if self.stopped() {
                break;
            }
            if self.now() < self.profile_state[i].next_at {
                continue;
            }
            let mut list = match self.cli.agents(&self.profiles[i].dir, &self.stop) {
                Ok(list) => list,
                Err(e) => {
                    self.agents_failed(i, &e);
                    continue;
                }
            };
            list.retain(|s| !is_own_relay(s, &self.relay_dir));
            let res = self
                .agents_ok(i)
                .map_err(Box::<dyn Error>::from)
                .and_then(|()| {
                    live += list.len();
                    self.apply_live(i, &list)
                });
            match res {
                Ok(n) => ended += n,
                Err(e) => {
                    // A store error for one profile must not stop the others; retried next pass.
                    self.counters.errors += 1;
                    let profile = text(self.profiles[i].id.as_str());
                    self.log.warn(
                        "live_apply_failed",
                        with_err(vec![("profile", profile)], e.as_ref()),
                    );
                }
            }
        }
        (live, ended)
    }

    fn agents_failed(&mut self, i: usize, e: &dyn Error) {
        if self.stopped() {
            return; // shutting down: no meta write, no retry
        }
        let now = self.now();
        let st = &mut self.profile_state[i];
        st.failures += 1;
        let delay = BACKOFF_MS[st.failures.min(BACKOFF_MS.len()) - 1];
        st.next_at = now + delay as i64;
        st.retry = Some(Instant::now() + Duration::from_millis(delay));
        let failures = st.failures;
        let profile = self.profiles[i].id.as_str();
        let key = format!("stale_since:{profile}");
        let stale = self.store.get_meta(&key).and_then(|v| match v {
            None => self.store.set_meta(&key, Some(&now.to_string())),
            Some(_) => Ok(()),
        });
        if let Err(err) = stale {
            self.log.warn("meta_failed", err_fields(&err));
        }
        self.log.warn(
            "agents_failed",
            with_err(
                vec![
                    ("profile", text(profile)),
                    ("failures", num(failures)),
                    ("backoffMs", num(delay as usize)),
                ],
                e,
            ),
        );
    }

    fn agents_ok(&mut self, i: usize) -> Result<(), StoreError> {
        let profile = self.profiles[i].id.as_str();
        let st = &mut self.profile_state[i];
        if st.failures > 0 {
            self.log
                .info("agents_recovered", vec![("profile", text(profile))]);
        }
        *st = ProfileState::default();
        let key = format!("stale_since:{profile}");
        if self.store.get_meta(&key)?.is_some() {
            self.store.set_meta(&key, None)?;
        }
        Ok(())
    }

    /// Upserts live sessions and marks this profile's missing ones ended. Returns the ended count.
    fn apply_live(&mut self, i: usize, list: &[LiveSession]) -> Result<usize, Box<dyn Error>> {
        let p = self.profiles[i].clone();
        let profile = p.id.as_str();
        let valid: Vec<&LiveSession> = list
            .iter()
            .filter(|s| is_session_id(&s.session_id))
            .collect();
        if valid.len() < list.len() {
            // Ids are joined into paths: never trust a non-uuid one. Count only, never the value.
            self.log.warn(
                "invalid_session_ids",
                vec![
                    ("profile", text(profile)),
                    ("count", num(list.len() - valid.len())),
                ],
            );
        }
        let mut rows: Vec<SessionRow> = Vec::new();
        let mut index: Option<HashMap<String, PathBuf>> = None;
        for s in &valid {
            let existing = self.row(&s.session_id);
            let mut path = existing
                .as_ref()
                .map_or_else(String::new, |r| r.transcript_path.clone());
            if path.is_empty() {
                if index.is_none() {
                    index = Some(transcript_index(&p.dir)?);
                }
                if let Some(found) = index.as_ref().and_then(|m| m.get(&s.session_id)) {
                    path = path_key(found);
                    self.missing_logged.remove(&s.session_id);
                }
            }
            let row = SessionRow {
                session_id: s.session_id.clone(),
                profile: profile.to_string(),
                name: s.name.clone(),
                cwd: s.cwd.clone(),
                pid: s.pid,
                kind: s.kind.clone(),
                status: s.status,
                started_at: s.started_at,
                updated_at: self.now(),
                ended_at: None,
                transcript_path: path,
                permission_mode: existing.as_ref().and_then(|r| r.permission_mode.clone()),
            };
            // Nothing the UI shows changed: leave the stored row (and its updated_at) alone.
            if existing.is_some_and(|e| same_session_data(&e, &row)) {
                continue;
            }
            rows.push(row);
        }
        let ids: Vec<String> = valid.iter().map(|s| s.session_id.clone()).collect();
        let id_set: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let at = self.now();
        let gone: Vec<SessionRow> = self
            .sessions
            .borrow()
            .values()
            .filter(|r| {
                r.profile == profile
                    && r.ended_at.is_none()
                    && !id_set.contains(r.session_id.as_str())
            })
            .cloned()
            .collect();
        if !rows.is_empty() || !gone.is_empty() {
            let store = &self.store;
            store.tx(|| -> Result<(), StoreError> {
                for r in &rows {
                    store.upsert_session(r)?;
                }
                for r in &gone {
                    store.mark_ended(&r.session_id, at)?;
                }
                Ok(())
            })?;
        }
        for r in rows {
            self.put_row(r);
        }
        let n = gone.len();
        for r in gone {
            self.put_row(SessionRow {
                ended_at: Some(at),
                updated_at: at,
                ..r
            });
        }
        *self.live_set(profile) = ids;
        Ok(n)
    }

    /// Indexes rows for recent transcripts not in the store; remembers them for tailing.
    fn scan_recent(&mut self) -> io::Result<()> {
        let mut recent = Vec::new();
        for p in self.profiles.clone() {
            for path in list_transcripts(&p.dir)? {
                if self.stopped() {
                    return Ok(());
                }
                let Some(session_id) = path
                    .file_name()
                    .map(|n| n.to_string_lossy())
                    .and_then(|n| n.strip_suffix(".jsonl").map(str::to_string))
                else {
                    continue;
                };
                // Only uuid-named transcripts are sessions; the id is later joined into paths.
                if !is_session_id(&session_id) {
                    continue;
                }
                if let Err(e) = self.scan_one(&p, &path, &session_id, &mut recent) {
                    self.counters.errors += 1;
                    self.log.warn(
                        "scan_error",
                        with_err(vec![("session", text(&session_id))], e.as_ref()),
                    );
                }
            }
        }
        self.recent = recent;
        Ok(())
    }

    fn scan_one(
        &mut self,
        p: &Profile,
        path: &Path,
        session_id: &str,
        recent: &mut Vec<(PathBuf, String)>,
    ) -> Result<(), Box<dyn Error>> {
        let meta = fs::metadata(path)?;
        let mtime = epoch_ms(meta.modified()?);
        let now = self.now();
        if now as f64 - mtime > RETENTION {
            return Ok(());
        }
        recent.push((path.to_path_buf(), session_id.to_string()));
        if let Some(existing) = self.row(session_id) {
            if existing.transcript_path.is_empty() {
                let row = SessionRow {
                    transcript_path: path_key(path),
                    ..existing
                };
                self.store.upsert_session(&row)?;
                self.put_row(row);
                self.missing_logged.remove(session_id);
            }
            return Ok(());
        }
        // TS `birthtimeMs || mtimeMs`: no (or a zero) creation time falls back to the mtime.
        let born = meta.created().map(epoch_ms).unwrap_or(0.0);
        let started = if born == 0.0 { mtime } else { born };
        let row = SessionRow {
            session_id: session_id.to_string(),
            profile: p.id.as_str().to_string(),
            name: String::new(),
            cwd: first_cwd(path)?,
            pid: 0,
            kind: "ended".to_string(),
            status: LiveStatus::Unknown,
            started_at: started.floor() as i64,
            updated_at: now,
            ended_at: Some(mtime.floor() as i64),
            transcript_path: path_key(path),
            permission_mode: None,
        };
        self.store.upsert_session(&row)?;
        self.put_row(row);
        Ok(())
    }

    fn tail_if_changed(&mut self, session_id: &str, path: &Path) {
        let meta = match fs::metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.transcript_gone(session_id, path);
                return;
            }
            Err(e) => {
                self.tail_error(session_id, &e);
                return;
            }
        };
        let size = meta.len();
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        let stamp = format!("{size}:{mtime}");
        let key = path_key(path);
        match self.store.get_tail(&key) {
            Ok(Some(t)) if t.size == size => {
                if t.offset == size {
                    return;
                }
                // offset < size is a budget-cut file (also after a restart: the map is empty then)
                // or a last line that is not complete yet. Only the latter is remembered, per
                // size+mtime.
                if self.partial_at.get(&key) == Some(&stamp) {
                    return;
                }
            }
            Ok(_) => {}
            Err(e) => {
                self.tail_error(session_id, &e);
                return;
            }
        }
        let errors = Cell::new(0usize);
        let res = {
            let Collector {
                store,
                sessions,
                now,
                log,
                on_records,
                ..
            } = self;
            let report = |e: &dyn Error| {
                errors.set(errors.get() + 1);
                log.warn(
                    "on_records_failed",
                    with_err(vec![("session", text(session_id))], e),
                );
            };
            let mut hook = on_records.as_mut().map(|f| {
                move |st: &Store, id: &str, recs: &[Classified]| {
                    if let Err(e) = f(st, id, recs) {
                        report(&e);
                    }
                }
            });
            let get_row = |id: &str| sessions.borrow().get(id).cloned();
            let row_changed = |row: &SessionRow| {
                sessions
                    .borrow_mut()
                    .insert(row.session_id.clone(), row.clone());
            };
            let clock = || now();
            let mut ctx = IngestCtx {
                store,
                get_row: &get_row,
                row_changed: &row_changed,
                now: &clock,
                on_records: hook
                    .as_mut()
                    .map(|h| h as &mut dyn FnMut(&Store, &str, &[Classified])),
            };
            ingest_file(&mut ctx, session_id, path, &report)
        };
        self.counters.errors += errors.get();
        let r = match res {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.transcript_gone(session_id, path);
                return;
            }
            Err(e) => {
                self.tail_error(session_id, &e);
                return;
            }
        };
        let partial = !r.more
            && matches!(self.store.get_tail(&key), Ok(Some(t)) if t.offset < t.size && t.size == size);
        if partial {
            self.partial_at.insert(key, stamp);
        } else {
            self.partial_at.remove(&key);
        }
        let c = &mut self.counters;
        c.files += 1;
        c.lines += r.lines;
        c.messages += r.messages;
        c.unknown += r.unknown;
        c.unrecognized += r.unrecognized;
    }

    fn tail_error(&mut self, session_id: &str, e: &dyn Error) {
        self.counters.errors += 1;
        self.log.warn(
            "tail_error",
            with_err(vec![("session", text(session_id))], e),
        );
    }

    /// Clears a vanished cached path so liveness or the scan finds the file again. Logs once.
    fn transcript_gone(&mut self, session_id: &str, path: &Path) {
        if let Some(row) = self.row(session_id)
            && row.transcript_path == path_key(path)
        {
            let next = SessionRow {
                transcript_path: String::new(),
                ..row
            };
            match self.store.upsert_session(&next) {
                Ok(()) => self.put_row(next),
                Err(e) => {
                    self.counters.errors += 1;
                    self.log.warn("store_error", err_fields(&e));
                }
            }
        }
        if self.missing_logged.insert(session_id.to_string()) {
            self.log
                .warn("transcript_missing", vec![("session", text(session_id))]);
        }
    }

    fn maintenance(&mut self) -> Result<(), StoreError> {
        let t = self.now();
        let last_purge = meta_time(self.store.get_meta("last_purge")?);
        // Busy: retried on the next pass. Any other error: at most once per MAINT_RETRY_MS.
        if t - last_purge >= PURGE_EVERY_MS && t >= self.purge_retry_at {
            let res = self.store.purge(t).and_then(|n| {
                self.store.set_meta("last_purge", Some(&t.to_string()))?;
                if n > 0 {
                    self.reload_sessions()?;
                }
                Ok(n)
            });
            match res {
                Ok(n) => {
                    self.purge_retry_at = 0;
                    self.log.info("purge", vec![("sessions", num(n))]);
                }
                Err(e) => {
                    let busy = is_busy(&e);
                    self.log.warn(
                        "purge_failed",
                        with_err(vec![("busy", LogValue::Bool(busy))], &e),
                    );
                    if busy {
                        return Ok(()); // contended: the vacuum waits for the next pass too
                    }
                    self.purge_retry_at = t + MAINT_RETRY_MS;
                }
            }
        }
        let last_vacuum = meta_time(self.store.get_meta("last_vacuum")?);
        if t - last_vacuum >= VACUUM_EVERY_MS && t >= self.vacuum_retry_at {
            let t0 = Instant::now();
            let res = self
                .store
                .vacuum()
                .and_then(|()| self.store.set_meta("last_vacuum", Some(&t.to_string())));
            match res {
                Ok(()) => {
                    self.vacuum_retry_at = 0;
                    let ms = (t0.elapsed().as_secs_f64() * 1000.0).round();
                    self.log.info("vacuum", vec![("ms", LogValue::Num(ms))]);
                }
                Err(e) => {
                    let busy = is_busy(&e);
                    self.log.warn(
                        "vacuum_failed",
                        with_err(vec![("busy", LogValue::Bool(busy))], &e),
                    );
                    if !busy {
                        self.vacuum_retry_at = t + MAINT_RETRY_MS;
                    }
                }
            }
        }
        Ok(())
    }
}

/// The collector loop. Refuses to start when the data dir is not private, when this process's own
/// start time cannot be read (see [`DaemonError::NoStartTime`]) or when another daemon holds the
/// lock. Returns after `deps.stop` is stopped and the in-flight pass finishes; the store is closed
/// before the lock is released.
pub fn run_daemon(deps: DaemonDeps) -> Result<(), DaemonError> {
    if !deps.platform.is_private_dir(&deps.data_dir)? {
        return Err(DaemonError::NotPrivate);
    }
    let run_dir = deps.data_dir.join("run");
    fs::create_dir_all(&run_dir)?;
    let started_at = deps
        .platform
        .process_start_time(std::process::id())
        .ok_or(DaemonError::NoStartTime)?;
    let lock = acquire_lock(
        &run_dir,
        LockOptions {
            identify: &*deps.daemon_identity,
            started_at: Some(started_at),
        },
    )?
    .ok_or(DaemonError::AlreadyRunning)?;
    // However the loop ends (also by a panic), the helper thread below is woken and ends.
    let stop = StopOnDrop(deps.stop.clone());
    let (tx, rx) = mpsc::channel();
    let mut collector = Collector::new(deps, tx.clone())?;
    let token = stop.0.clone();
    std::thread::spawn(move || {
        while !token.wait_timeout(STOP_WAIT) {}
        let _ = tx.send(Msg::Stop);
    });
    collector.run(&rx);
    drop(collector); // closes the store
    drop(stop);
    lock.release();
    Ok(())
}
