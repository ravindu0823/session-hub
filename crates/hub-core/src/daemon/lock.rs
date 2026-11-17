//! Single-instance lock at `<run>/daemon.lock`. Port of `src/daemon/lock.ts`; the file format is
//! byte-identical so the Rust and TS daemons share it during the transition.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::platform::Platform;
use crate::text::is_js_space;

pub const LOCK_FILE: &str = "daemon.lock";
/// A lock file without a valid pid younger than this may still be mid-write by its creator.
const FRESH_MS: u128 = 5_000;
/// A recorded start time matches a live process's start time within this much.
///
/// Linux caveat: the start time there is `btime + starttime / HZ`, read from `/proc`. It is stable
/// from one read to the next, but `btime` follows the wall clock, so a clock step of more than 2 s
/// after a lock was written makes a live daemon's start time differ from the recorded one: it then
/// looks "foreign" and its lock can be taken over. TS has the same rule; Windows and macOS report
/// a creation time that does not move with the clock.
pub const START_TIME_SLACK_MS: i64 = 2_000;
/// Number.MAX_SAFE_INTEGER: TS refuses larger numbers in a lock.
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// What daemon.lock records: the daemon's pid and its process start time (epoch ms). A lock written
/// by an older version has only the pid (`started_at` None).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockInfo {
    pub pid: u32,
    pub started_at: Option<i64>,
}

/// A run of ASCII digits as a JS safe integer (`Number(digits)`), else None.
fn safe_integer(digits: &str) -> Option<u64> {
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Leading zeros are a plain number in JS; strip them so a long zero prefix cannot overflow.
    let trimmed = digits.trim_start_matches('0');
    if trimmed.is_empty() {
        return Some(0);
    }
    trimmed
        .parse::<u64>()
        .ok()
        .filter(|&n| n <= MAX_SAFE_INTEGER)
}

/// Pure: `<pid>` (legacy) or `<pid> <startedAtMs>` after a JS `trim`; None for anything else (also
/// a pid beyond u32, which no process can have).
pub fn parse_lock(text: &str) -> Option<LockInfo> {
    let t = text.trim_matches(is_js_space);
    let (pid, started) = match t.split_once(' ') {
        Some((p, s)) => (p, Some(s)),
        None => (t, None),
    };
    let pid = safe_integer(pid)?;
    if pid == 0 {
        return None;
    }
    let pid = u32::try_from(pid).ok()?;
    let started_at = match started {
        None => None,
        Some(s) => Some(i64::try_from(safe_integer(s)?).ok()?),
    };
    Some(LockInfo { pid, started_at })
}

/// Pure: the lock file text for `info`, with no trailing newline (TS `formatLock`).
pub fn format_lock(info: &LockInfo) -> String {
    match info.started_at {
        None => info.pid.to_string(),
        Some(t) => format!("{} {t}", info.pid),
    }
}

/// What the process named in daemon.lock is:
/// - `Ours`: the daemon that wrote the lock. With a recorded start time: the pid is alive and
///   started at that time (within [`START_TIME_SLACK_MS`]), whatever its image (a daemon run from
///   source is bun.exe). With a legacy pid-only lock: alive and running a daemon image (hubd.exe,
///   hub.exe, hubw.exe, `hub` on Unix), or bun.exe when this process itself runs from source (`dev`).
/// - `Foreign`: dead, or alive but a different process: another start time, or (legacy lock) a
///   readable image that is none of those. Only this is a stale lock.
/// - `Unknown`: alive but it cannot be queried (elevated, protected, gone mid-query), or (legacy
///   lock) bun.exe while this process is compiled.
///
/// Start and lock decisions treat `Ours` and `Unknown` as held: never a second daemon, never a
/// takeover. Only `Ours` may ever be killed (and only after [`is_killable_image`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonIdentity {
    Ours,
    Foreign,
    Unknown,
}

/// What can be learned about a live pid; each is None when it cannot be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessProbe {
    pub alive: bool,
    pub started_at: Option<i64>,
    pub image: Option<PathBuf>,
}

/// Lower-cased base name of an image path, with `\` read as a separator on every OS.
fn image_name(image: &Path) -> String {
    let s = image.to_string_lossy().replace('\\', "/");
    // Node `basename` ignores trailing separators.
    let base = s.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    base.to_lowercase()
}

/// A name the compiled daemon runs as: hubd.exe / hub.exe (TS), hubw.exe (the Rust windowless
/// build), and `hub` on Unix.
fn is_daemon_name(name: &str) -> bool {
    matches!(name, "hubd.exe" | "hub.exe" | "hubw.exe") || (cfg!(unix) && name == "hub")
}

/// Pure: classifies the lock's process from what was probed about it.
pub fn daemon_identity(lock: &LockInfo, probe: &ProcessProbe, dev: bool) -> DaemonIdentity {
    if !probe.alive {
        return DaemonIdentity::Foreign;
    }
    if let Some(recorded) = lock.started_at {
        return match probe.started_at {
            None => DaemonIdentity::Unknown,
            Some(t) if t.abs_diff(recorded) <= START_TIME_SLACK_MS as u64 => DaemonIdentity::Ours,
            Some(_) => DaemonIdentity::Foreign,
        };
    }
    let Some(image) = &probe.image else {
        return DaemonIdentity::Unknown;
    };
    let name = image_name(image);
    if is_daemon_name(&name) {
        DaemonIdentity::Ours
    } else if name == "bun.exe" {
        if dev {
            DaemonIdentity::Ours
        } else {
            DaemonIdentity::Unknown
        }
    } else {
        DaemonIdentity::Foreign
    }
}

/// True for an image a daemon can run as (hubd.exe, hub.exe, hubw.exe, bun.exe, and `hub` on
/// Unix; base name, any case): the last guard before a kill.
pub fn is_killable_image(image: Option<&Path>) -> bool {
    image.is_some_and(|i| {
        let name = image_name(i);
        is_daemon_name(&name) || name == "bun.exe"
    })
}

/// [`daemon_identity`] for a lock: queries a live process only, and only for what the lock format
/// needs (the start time, or the image for a legacy lock).
pub fn identify_daemon(lock: &LockInfo, platform: &dyn Platform, dev: bool) -> DaemonIdentity {
    if !platform.process_alive(lock.pid) {
        return DaemonIdentity::Foreign;
    }
    let probe = if lock.started_at.is_some() {
        ProcessProbe {
            alive: true,
            started_at: platform.process_start_time(lock.pid),
            image: None,
        }
    } else {
        ProcessProbe {
            alive: true,
            started_at: None,
            image: platform.process_image_path(lock.pid),
        }
    };
    daemon_identity(lock, &probe, dev)
}

/// `O_EXCL` create holding `text`; false when the file exists.
fn try_create(path: &Path, text: &str) -> io::Result<bool> {
    let mut f = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(e),
    };
    f.write_all(text.as_bytes())?;
    Ok(true)
}

/// File text decoded like Node's `readFileSync(path, "utf8")`.
fn read_text(path: &Path) -> io::Result<String> {
    Ok(String::from_utf8_lossy(&fs::read(path)?).into_owned())
}

/// The text of a lock judged stale (dead holder, reused pid, or an old unparseable file), or None
/// while it is held. `""` when the file vanished between create and read.
fn stale_text(path: &Path, identify: &dyn Fn(&LockInfo) -> DaemonIdentity) -> Option<String> {
    let Ok(text) = read_text(path) else {
        return Some(String::new()); // vanished between create and read: stale
    };
    if let Some(lock) = parse_lock(&text) {
        return (identify(&lock) == DaemonIdentity::Foreign).then_some(text);
    }
    let Ok(modified) = fs::metadata(path).and_then(|m| m.modified()) else {
        return Some(String::new());
    };
    // A modification time in the future counts as fresh, like TS's negative age.
    let fresh = match SystemTime::now().duration_since(modified) {
        Ok(age) => age.as_millis() < FRESH_MS,
        Err(_) => true,
    };
    if fresh { None } else { Some(text) }
}

pub struct LockOptions<'a> {
    /// Classifies the process of an existing lock (the daemon passes [`identify_daemon`]). TS's
    /// default, "a live pid is unknown, a dead one foreign", needs a liveness check, so it is the
    /// caller's to pass.
    pub identify: &'a dyn Fn(&LockInfo) -> DaemonIdentity,
    /// This process's start time (epoch ms), written next to its pid; None writes the pid only.
    pub started_at: Option<i64>,
}

/// A held lock. Dropping it (or [`LockGuard::release`]) deletes the file, but only while it still
/// holds our text: a lock another process has taken over since is left alone.
#[derive(Debug)]
pub struct LockGuard {
    path: PathBuf,
    mine: String,
}

impl LockGuard {
    pub fn release(self) {
        drop(self);
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if let Ok(text) = read_text(&self.path)
            && text.trim_matches(is_js_space) == self.mine
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Single-instance lock at `<dir>/daemon.lock`, holding `<pid> <startedAtMs>` (or `<pid>` when the
/// start time is unknown). `Ok(None)` while it is held. Only a `Foreign` holder (dead, or a pid
/// reused by another process after an unclean exit) leaves a stale lock, which is taken over. An
/// error (a missing or unwritable `dir`) is returned, as TS throws it.
pub fn acquire_lock(dir: &Path, opts: LockOptions<'_>) -> io::Result<Option<LockGuard>> {
    let mine = format_lock(&LockInfo {
        pid: std::process::id(),
        started_at: opts.started_at,
    });
    let path = dir.join(LOCK_FILE);
    if !try_create(&path, &mine)? {
        let Some(stale) = stale_text(&path, opts.identify) else {
            return Ok(None);
        };
        // Compare-and-delete: remove only the exact lock judged stale, so a rival that took it over
        // before this read keeps its fresh lock (RES-11). Read and unlink are two steps, so a rival
        // that takes over between them can still lose its lock; this narrows the window, it does
        // not close it. An exclusive handle held for the daemon's life would.
        if read_text(&path).is_ok_and(|t| t == stale) {
            let _ = fs::remove_file(&path);
        }
        if !try_create(&path, &mine)? {
            return Ok(None);
        }
    }
    Ok(Some(LockGuard { path, mine }))
}
