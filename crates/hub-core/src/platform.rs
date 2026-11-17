//! OS access behind a trait, so everything else is testable with the fake. The real
//! implementations live in the `hub-platform` crate (`hub_platform::native()`).
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::discovery::StopToken;

/// What bringing a session's terminal window to the front did. It only ever focuses a window; it
/// never types into it or answers anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusResult {
    /// The window with this title was brought to the front.
    Focused { title: String },
    /// Not focused. `reason` is TS's: `no-window`, `focus-denied` or `error`.
    NotFound { reason: String },
    /// This OS has no focus support.
    Unsupported,
}

/// Removes stop-signal handlers when dropped (see [`Platform::stop_on_signals`]).
pub struct SignalGuard(Option<Box<dyn FnOnce() + Send>>);

impl SignalGuard {
    /// A guard that runs `remove` once, when dropped.
    pub fn new(remove: impl FnOnce() + Send + 'static) -> SignalGuard {
        SignalGuard(Some(Box::new(remove)))
    }

    /// A guard with nothing to remove.
    pub fn none() -> SignalGuard {
        SignalGuard(None)
    }
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        if let Some(remove) = self.0.take() {
            remove();
        }
    }
}

impl std::fmt::Debug for SignalGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SignalGuard")
    }
}

pub trait Platform: Send + Sync {
    /// Creates `dir` (and its parents) and makes it readable by the current user only (and SYSTEM
    /// on Windows). Errors when that cannot be verified afterwards.
    fn ensure_private_dir(&self, dir: &Path) -> io::Result<()>;
    /// True only for an existing directory nobody but the current user (and SYSTEM) can access;
    /// false when it is missing or cannot be inspected.
    fn is_private_dir(&self, dir: &Path) -> io::Result<bool>;
    /// True while a process with this pid exists (also when we may not query or signal it).
    fn process_alive(&self, pid: u32) -> bool;
    /// When the process started, epoch ms; None when it is gone or cannot be queried.
    fn process_start_time(&self, pid: u32) -> Option<i64>;
    /// Full path of the process's executable; None when it is gone or cannot be queried.
    fn process_image_path(&self, pid: u32) -> Option<PathBuf>;
    /// Prepares a child command to run without a console window (Windows) and in its own process
    /// group, so [`Platform::kill_tree`] reaches its children.
    fn spawn_hidden(&self, cmd: &mut Command);
    /// Kills the process and its children.
    fn kill_tree(&self, pid: u32) -> io::Result<()>;
    /// Brings the terminal window that hosts this pid (or its nearest ancestor with a window) to the
    /// front.
    fn focus_session_window(&self, pid: u32) -> FocusResult;
    /// Until the guard is dropped, Ctrl-C and SIGTERM (on Windows: Ctrl-C, Ctrl-Break and closing
    /// the console) call `stop.stop()` instead of ending the process, so the daemon stops cleanly
    /// and releases its lock. The default (the fake) installs nothing.
    fn stop_on_signals(&self, stop: StopToken) -> io::Result<SignalGuard> {
        let _ = stop;
        Ok(SignalGuard::none())
    }
}

/// True for a pid no tree kill may target: this process, pid 0 (on Unix our own process group, on
/// Windows the idle process), on Unix pid 1 (`killpg(1)` is `kill(-1)`, a signal to every process
/// we may signal) and on Windows pid 4 (System). Every [`Platform::kill_tree`] refuses these.
pub fn is_dangerous_kill_pid(pid: u32) -> bool {
    pid == 0 || pid == std::process::id() || (cfg!(unix) && pid == 1) || (cfg!(windows) && pid == 4)
}

/// For tests: hidden spawn does nothing; `kill_tree` really kills the process (and on Windows its
/// children, through `taskkill /T`), so tests of timeouts and stops end their fake children. Every
/// dir is private and `ensure_private_dir` does nothing; only the own pid is alive, and no start
/// time or image is known (like TS's fake); focus is unsupported. Dangerous pids
/// ([`is_dangerous_kill_pid`]) are refused like the real platforms do.
#[derive(Debug, Default, Clone, Copy)]
pub struct FakePlatform;

impl Platform for FakePlatform {
    fn ensure_private_dir(&self, _dir: &Path) -> io::Result<()> {
        Ok(())
    }

    fn is_private_dir(&self, _dir: &Path) -> io::Result<bool> {
        Ok(true)
    }

    fn process_alive(&self, pid: u32) -> bool {
        pid == std::process::id()
    }

    fn process_start_time(&self, _pid: u32) -> Option<i64> {
        None
    }

    fn process_image_path(&self, _pid: u32) -> Option<PathBuf> {
        None
    }

    fn spawn_hidden(&self, _cmd: &mut Command) {}

    fn kill_tree(&self, pid: u32) -> io::Result<()> {
        if is_dangerous_kill_pid(pid) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("kill_tree refuses pid {pid}"),
            ));
        }
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new(crate::sysbin::taskkill());
            c.args(["/PID", &pid.to_string(), "/T", "/F"]);
            c
        } else {
            let mut c = Command::new("kill");
            c.args(["-9", &pid.to_string()]);
            c
        };
        let status = cmd
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!("kill_tree {pid} exited {status}")))
        }
    }

    fn focus_session_window(&self, _pid: u32) -> FocusResult {
        FocusResult::Unsupported
    }
}
