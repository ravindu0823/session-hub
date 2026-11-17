//! Process queries (sysinfo for the start time and image, `kill(pid, 0)` for liveness) and the
//! process-group kill.
use std::io;
use std::path::PathBuf;

use rustix::io::Errno;
use rustix::process::{
    Pid, Signal, getpgid, getpgrp, kill_process, kill_process_group, test_kill_process,
};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

fn rustix_pid(pid: u32) -> Option<Pid> {
    Pid::from_raw(i32::try_from(pid).ok()?)
}

/// `kill(pid, 0)`: true when the process exists, also when we may not signal it (EPERM), like TS's
/// `isAlive`.
pub fn alive(pid: u32) -> bool {
    let Some(p) = rustix_pid(pid) else {
        return false;
    };
    match test_kill_process(p) {
        Ok(()) => true,
        Err(e) => e == Errno::PERM,
    }
}

/// Refreshes only `pid` (and its exe when asked), so a call stays cheap.
fn with_process<T>(
    pid: u32,
    kind: ProcessRefreshKind,
    f: impl FnOnce(&sysinfo::Process) -> Option<T>,
) -> Option<T> {
    rustix_pid(pid)?;
    let mut sys = System::new().ok()?;
    let spid = sysinfo::Pid::from_u32(pid);
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[spid]), true, kind);
    f(sys.process(spid)?)
}

/// Epoch ms (sysinfo's start time is in whole seconds); None when gone or not queryable.
pub fn start_time(pid: u32) -> Option<i64> {
    with_process(pid, ProcessRefreshKind::nothing(), |p| {
        let secs = i64::try_from(p.start_time()).ok()?;
        (secs > 0).then(|| secs.saturating_mul(1000))
    })
}

pub fn image_path(pid: u32) -> Option<PathBuf> {
    with_process(
        pid,
        ProcessRefreshKind::nothing().with_exe(UpdateKind::Always),
        |p| p.exe().map(PathBuf::from),
    )
}

/// SIGKILL to the process group `pid` leads (a child spawned hidden has pgid == pid); a pid that
/// leads no group of its own is killed alone.
///
/// When `getpgid` gives ESRCH the group is still tried: macOS reports ESRCH for an exited but
/// unreaped leader (a zombie) whose group still holds live grandchildren. That is safe because a
/// pgid cannot be reused while its group has members. Only when the group kill also finds nothing
/// is the pid killed alone (an error when it is gone too). Refuses pid 0, our own pid and our own
/// process group, so it never signals this process, and pid 1: `getpgid(1)` is 1, and `killpg(1)`
/// is `kill(-1)`, which signals every process we may signal (ruling R11).
pub fn kill_tree(pid: u32) -> io::Result<()> {
    let refuse = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("kill_tree refuses pid {pid}"),
        )
    };
    if hub_core::platform::is_dangerous_kill_pid(pid) {
        return Err(refuse());
    }
    let p = rustix_pid(pid).ok_or_else(refuse)?;
    if p == getpgrp() {
        return Err(refuse());
    }
    let try_group = match getpgid(Some(p)) {
        Ok(pgid) => pgid == p,
        Err(e) => e == Errno::SRCH,
    };
    if try_group {
        match kill_process_group(p, Signal::KILL) {
            Ok(()) => return Ok(()),
            Err(e) if e == Errno::SRCH || e == Errno::PERM => {}
            Err(e) => return Err(e.into()),
        }
    }
    kill_process(p, Signal::KILL).map_err(io::Error::from)
}
