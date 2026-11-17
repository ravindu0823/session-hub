//! `hub daemon run|start|stop|status`. Port of `cmdDaemon`, `startDaemon` and `stopDaemon` in
//! `src/cli/commands.ts`.
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hub_core::config::{Config, DAEMON_STOPPED_FILE};
use hub_core::daemon::collector::real_watch;
use hub_core::daemon::lock::{
    DaemonIdentity, LOCK_FILE, LockInfo, identify_daemon, is_killable_image, parse_lock,
};
use hub_core::daemon::run::{DaemonRunDeps, RunOutcome, daemon_run};

use crate::cli::CliDeps;

const WAIT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(100);

fn config(d: &CliDeps) -> Result<&Config, i32> {
    d.config.as_ref().map_err(|e| {
        (d.err)(&format!("hub: {e}"));
        1
    })
}

fn identify(d: &CliDeps, lock: &LockInfo) -> DaemonIdentity {
    // Compiled (never a dev run through bun): bun.exe is not a daemon image here.
    identify_daemon(lock, &*d.platform, false)
}

fn read_lock(run_dir: &Path) -> Option<LockInfo> {
    let bytes = fs::read(run_dir.join(LOCK_FILE)).ok()?;
    parse_lock(&String::from_utf8_lossy(&bytes))
}

/// The lock held by our daemon, or by a live process that cannot be verified (never start a
/// second daemon beside it). None for no lock, a dead pid, or a pid another program has now.
fn held_lock(d: &CliDeps, run_dir: &Path) -> Option<LockInfo> {
    let lock = read_lock(run_dir)?;
    (identify(d, &lock) != DaemonIdentity::Foreign).then_some(lock)
}

fn wait_for<T>(mut probe: impl FnMut() -> Option<T>) -> Option<T> {
    let start = Instant::now();
    loop {
        if let Some(v) = probe() {
            return Some(v);
        }
        if start.elapsed() >= WAIT {
            return None;
        }
        std::thread::sleep(POLL);
    }
}

pub fn status(d: &CliDeps) -> i32 {
    let config = match config(d) {
        Ok(c) => c,
        Err(code) => return code,
    };
    match held_lock(d, &config.run_dir()) {
        Some(lock) => {
            (d.out)(&format!("RUNNING pid={}", lock.pid));
            0
        }
        None => {
            (d.out)("STOPPED");
            3
        }
    }
}

/// The daemon's command line: `hub daemon run` (Unix), `hubw.exe daemon run` next to this exe
/// (Windows), falling back to this exe.
fn daemon_exe(exe: &Path) -> (PathBuf, bool) {
    if cfg!(windows) {
        let hubw = exe.with_file_name("hubw.exe");
        if hubw.is_file() {
            return (hubw, true);
        }
    }
    (exe.to_path_buf(), false)
}

/// Starts `daemon run` detached from this process and its terminal, with no console window and
/// stdio null, so it survives this command and the console closing.
fn spawn_daemon(d: &CliDeps) -> io::Result<()> {
    let (exe, windowless) = daemon_exe(&d.exe);
    let mut cmd = Command::new(&exe);
    cmd.args(["daemon", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Its own folder, so the daemon never pins the caller's.
    if let Some(dir) = exe.parent().filter(|p| !p.as_os_str().is_empty()) {
        cmd.current_dir(dir);
    }
    detach(d, &mut cmd, windowless);
    #[cfg(windows)]
    let _restore = UninheritedStdHandles::new();
    let child = cmd.spawn()?;
    // Unix: reaped here, or it stays a zombie for as long as a long-lived caller (the TUI) runs.
    #[cfg(unix)]
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    #[cfg(windows)]
    drop(child);
    Ok(())
}

#[cfg(unix)]
fn detach(_d: &CliDeps, cmd: &mut Command, _windowless: bool) {
    use std::os::unix::process::CommandExt;
    // A new session: no controlling terminal and no SIGHUP when it closes. It leads its own
    // process group (pgid = pid), so `kill_tree` reaches its children. `spawn_hidden`'s
    // `process_group(0)` must not run first: a group leader cannot call setsid.
    // SAFETY: setsid is async-signal-safe and touches no memory of ours.
    unsafe {
        cmd.pre_exec(|| rustix::process::setsid().map(drop).map_err(io::Error::from));
    }
}

/// Windows hands every inheritable handle to the child, not only the stdio it is given. Our own
/// stdout/stderr may be a caller's pipe (the tray's bridge, a test): if the daemon inherited it, the
/// caller would wait for end of file until the daemon exits. So the std handles are made not
/// inheritable for the spawn, and put back as they were when the guard drops (also after a failed
/// spawn): a long-lived caller (the TUI) keeps its own stdio inheritable.
#[cfg(windows)]
pub(crate) struct UninheritedStdHandles(Vec<(windows_sys::Win32::Foundation::HANDLE, u32)>);

#[cfg(windows)]
impl UninheritedStdHandles {
    pub(crate) fn new() -> Self {
        use windows_sys::Win32::Foundation::{
            GetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
        };
        use windows_sys::Win32::System::Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };
        let mut saved = Vec::new();
        for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: plain Win32 calls on this process's own standard handles; a failure only
            // leaves the handle as it was.
            unsafe {
                let h = GetStdHandle(id);
                let mut flags = 0u32;
                if !h.is_null()
                    && h != INVALID_HANDLE_VALUE
                    && GetHandleInformation(h, &mut flags) != 0
                {
                    saved.push((h, flags & HANDLE_FLAG_INHERIT));
                    SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
                }
            }
        }
        UninheritedStdHandles(saved)
    }
}

#[cfg(windows)]
impl Drop for UninheritedStdHandles {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
        for &(h, inherit) in &self.0 {
            // SAFETY: the handle was valid when saved; if it was closed since, the call fails.
            unsafe {
                SetHandleInformation(h, HANDLE_FLAG_INHERIT, inherit);
            }
        }
    }
}

#[cfg(windows)]
fn detach(d: &CliDeps, cmd: &mut Command, windowless: bool) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    if windowless {
        // hubw is a GUI-subsystem exe: it has no console to inherit or show.
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    } else {
        // hub.exe is a console exe: CREATE_NO_WINDOW and its own process group.
        d.platform.spawn_hidden(cmd);
    }
}

/// How [`ensure_started`] ended.
pub enum Started {
    /// Our daemon already held the lock.
    Already(u32),
    Started(u32),
    /// The process could not be spawned.
    SpawnFailed(String),
    /// Spawned, but no lock appeared within 5 s.
    TimedOut,
}

/// Starts the daemon unless one runs (`startDaemon` in commands.ts); prints nothing. A start ends
/// an earlier deliberate stop, running or not.
pub fn ensure_started(d: &CliDeps, run_dir: &Path) -> Started {
    let held = held_lock(d, run_dir);
    let _ = fs::remove_file(run_dir.join(DAEMON_STOPPED_FILE));
    if let Some(lock) = held {
        return Started::Already(lock.pid);
    }
    if let Err(e) = spawn_daemon(d) {
        return Started::SpawnFailed(e.to_string());
    }
    match wait_for(|| held_lock(d, run_dir)) {
        Some(lock) => Started::Started(lock.pid),
        None => Started::TimedOut,
    }
}

/// The TUI's warning when the daemon is not running and did not start.
pub fn start_warning(run_dir: &Path) -> String {
    format!(
        "The daemon is not running and did not start; the list may be stale. See {}",
        daemon_log_path(run_dir).display()
    )
}

pub fn daemon_log_path(run_dir: &Path) -> PathBuf {
    run_dir.join("daemon.log")
}

pub fn start(d: &CliDeps) -> i32 {
    let config = match config(d) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let run_dir = config.run_dir();
    match ensure_started(d, &run_dir) {
        Started::Already(pid) => {
            (d.out)(&format!("ALREADY RUNNING pid={pid}"));
            0
        }
        Started::SpawnFailed(e) => {
            (d.err)(&format!("hub: cannot start the daemon: {e}"));
            1
        }
        Started::Started(pid) => {
            (d.out)(&format!("STARTED pid={pid}"));
            0
        }
        Started::TimedOut => {
            (d.err)(&format!(
                "FAILED: the daemon did not start within 5 s; see {}",
                daemon_log_path(&run_dir).display()
            ));
            1
        }
    }
}

/// The check `hub daemon status` makes, as a closure the TUI asks on every refresh: true while a
/// lock names our daemon (or a live process that cannot be verified).
pub fn daemon_running_probe(d: &CliDeps, run_dir: PathBuf) -> Box<dyn Fn() -> bool> {
    let platform = d.platform.clone();
    Box::new(move || {
        read_lock(&run_dir).is_some_and(|lock| {
            identify_daemon(&lock, &*platform, false) != DaemonIdentity::Foreign
        })
    })
}

/// True when stopped (or never running). Only our daemon is killed: its identity (`Ours`) and its
/// image are both checked right before the kill. A lock naming another process is stale and never
/// acted on; one that cannot be verified is left running (false).
fn stop_daemon(d: &CliDeps, run_dir: &Path) -> bool {
    let Some(lock) = held_lock(d, run_dir) else {
        return true;
    };
    let pid = lock.pid;
    if identify(d, &lock) != DaemonIdentity::Ours
        || !is_killable_image(d.platform.process_image_path(pid).as_deref())
    {
        (d.err)(&format!(
            "daemon: pid={pid} holds daemon.lock but cannot be verified as the daemon (it may run \
             elevated); not stopped. Stop it by hand."
        ));
        return false;
    }
    let _ = d.platform.kill_tree(pid);
    if wait_for(|| (identify(d, &lock) == DaemonIdentity::Foreign).then_some(())).is_none() {
        return false;
    }
    // A killed daemon cannot release its lock; drop it if it still names that pid.
    if read_lock(run_dir).is_some_and(|l| l.pid == pid) {
        let _ = fs::remove_file(run_dir.join(LOCK_FILE));
    }
    true
}

pub fn stop(d: &CliDeps) -> i32 {
    let config = match config(d) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let run_dir = config.run_dir();
    let pid = held_lock(d, &run_dir).map(|l| l.pid);
    // First, so the tray's bridge does not start the daemon again while it goes down.
    if let Err(e) =
        fs::create_dir_all(&run_dir).and_then(|()| fs::write(run_dir.join(DAEMON_STOPPED_FILE), ""))
    {
        (d.err)(&format!("hub: cannot write {DAEMON_STOPPED_FILE}: {e}"));
        return 1;
    }
    if stop_daemon(d, &run_dir) {
        (d.out)("STOPPED");
        return 0;
    }
    (d.err)(&format!(
        "FAILED: pid={} is still running",
        pid.map_or_else(|| "?".to_string(), |p| p.to_string())
    ));
    1
}

/// The daemon in this process, until Ctrl-C or SIGTERM.
pub fn run(d: &CliDeps) -> i32 {
    let config = match config(d) {
        Ok(c) => c.clone(),
        Err(code) => return code,
    };
    let platform = d.platform.clone();
    let identity_platform = platform.clone();
    let outcome = daemon_run(DaemonRunDeps {
        config,
        platform,
        cli: d.cli.clone(),
        now: Arc::new(now_ms),
        daemon_identity: Arc::new(move |lock| identify_daemon(lock, &*identity_platform, false)),
        watch: real_watch(),
        run_daemon: None,
    });
    match outcome {
        RunOutcome::Stopped => 0,
        RunOutcome::AlreadyRunning => {
            (d.out)("ALREADY RUNNING");
            0
        }
        RunOutcome::Failed(msg) => {
            (d.err)(&format!("FAILED: {msg}"));
            1
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |t| i64::try_from(t.as_millis()).unwrap_or(i64::MAX))
}
