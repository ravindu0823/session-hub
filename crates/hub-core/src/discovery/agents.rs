//! `claude agents --json` per profile: the live session list. Port of `src/discovery/agents.ts`.
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{AGENTS_TIMEOUT_MS, Os, RELAY_NAME, current_os};
use crate::platform::Platform;
use crate::store::LiveStatus;

/// `claude agents` output beyond this is refused (and the child killed): memory stays bounded.
pub const MAX_AGENTS_STDOUT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveSession {
    pub pid: i64,
    pub cwd: String,
    pub kind: String,
    pub started_at: i64,
    pub session_id: String,
    pub name: String,
    pub status: LiveStatus,
}

/// Error texts match the TS messages; none contains output from the child.
#[derive(Debug, thiserror::Error)]
pub enum AgentsError {
    #[error("claude agents aborted")]
    Aborted,
    #[error("claude agents timed out")]
    TimedOut,
    #[error("claude agents output is over 4 MiB")]
    TooLarge,
    #[error("claude agents exited {0}")]
    Exited(i32),
    #[error("claude agents returned invalid JSON")]
    InvalidJson,
    #[error("claude agents could not start: {0}")]
    Spawn(std::io::Error),
}

/// A flag a running call watches: the daemon's stop. Clones share the flag.
#[derive(Debug, Clone, Default)]
pub struct StopToken(Arc<(Mutex<bool>, Condvar)>);

impl StopToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn stop(&self) {
        let (lock, cv) = &*self.0;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cv.notify_all();
    }

    pub fn is_stopped(&self) -> bool {
        *self.0.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Sleeps up to `d`, waking early when stopped. True when stopped.
    pub fn wait_timeout(&self, d: Duration) -> bool {
        let (lock, cv) = &*self.0;
        let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let (guard, _) = cv
            .wait_timeout_while(guard, d, |stopped| !*stopped)
            .unwrap_or_else(|e| e.into_inner());
        *guard
    }
}

pub trait ClaudeCli: Send + Sync {
    /// `stop` (the daemon's stop) kills a running child and returns [`AgentsError::Aborted`].
    fn agents(&self, profile_dir: &Path, stop: &StopToken)
    -> Result<Vec<LiveSession>, AgentsError>;
}

/// Like TS `parseStatus`.
pub fn parse_status(v: &Value) -> LiveStatus {
    match v.as_str() {
        Some("busy") => LiveStatus::Busy,
        Some("idle") => LiveStatus::Idle,
        Some("waiting") => LiveStatus::Waiting,
        _ => LiveStatus::Unknown,
    }
}

/// Any JSON number, truncated toward zero like a JS number used as an integer. None when it does
/// not fit an i64.
fn number_to_i64(v: &Value) -> Option<i64> {
    let n = v.as_number()?;
    if let Some(i) = n.as_i64() {
        return Some(i);
    }
    let f = n.as_f64()?;
    // 2^63 is exactly representable; the range is [-2^63, 2^63).
    if f.is_finite() && (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&f) {
        Some(f.trunc() as i64)
    } else {
        None
    }
}

fn string_or(o: &serde_json::Map<String, Value>, key: &str, fallback: &str) -> String {
    o.get(key)
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .to_string()
}

/// Tolerant: entries without a string sessionId or a numeric pid are skipped.
pub fn parse_agents(v: &Value) -> Vec<LiveSession> {
    let Some(items) = v.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in items {
        let Some(o) = e.as_object() else { continue };
        let Some(session_id) = o.get("sessionId").and_then(Value::as_str) else {
            continue;
        };
        let Some(pid) = o.get("pid").and_then(number_to_i64) else {
            continue;
        };
        out.push(LiveSession {
            pid,
            cwd: string_or(o, "cwd", ""),
            kind: string_or(o, "kind", "unknown"),
            started_at: o.get("startedAt").and_then(number_to_i64).unwrap_or(0),
            session_id: session_id.to_string(),
            name: string_or(o, "name", ""),
            status: o.get("status").map_or(LiveStatus::Unknown, parse_status),
        });
    }
    out
}

/// Lexical normalisation like `path.resolve` plus trailing-separator trimming; lower-cased on Windows.
fn normalize_path(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if current_os() == Os::Windows {
        PathBuf::from(out.to_string_lossy().to_lowercase())
    } else {
        out
    }
}

pub fn is_own_relay(s: &LiveSession, relay_dir: &Path) -> bool {
    if s.name != RELAY_NAME || s.cwd.is_empty() {
        return false;
    }
    let cwd = Path::new(&s.cwd);
    if !cwd.is_absolute() {
        return false;
    }
    normalize_path(cwd) == normalize_path(relay_dir)
}

#[cfg(unix)]
fn is_runnable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_runnable(p: &Path) -> bool {
    p.is_file()
}

/// Searches a PATH value: `claude.exe`, then `claude.cmd` on Windows; `claude` elsewhere.
pub fn find_claude(env_path: &OsStr) -> Option<PathBuf> {
    let names: &[&str] = if cfg!(windows) {
        &["claude.exe", "claude.cmd"]
    } else {
        &["claude"]
    };
    for dir in std::env::split_paths(env_path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for name in names {
            let candidate = dir.join(name);
            if is_runnable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

pub struct RealClaudeCli {
    pub exe: PathBuf,
    pub platform: Arc<dyn Platform>,
}

enum Reader {
    Done(Vec<u8>),
    TooLarge,
}

const POLL: Duration = Duration::from_millis(25);

impl RealClaudeCli {
    fn kill(&self, child: &mut Child) {
        let _ = self.platform.kill_tree(child.id());
        // Always, whatever kill_tree reported (ruling R9): a tree kill that returned Ok but left the
        // root alive must not make the wait below block. An error here means it is already gone.
        let _ = child.kill();
        // Reap it; the child is dead or dying, so this returns at once.
        let _ = child.wait();
    }

    /// `agents` with an explicit timeout (tests use short ones).
    pub fn agents_with_timeout(
        &self,
        profile_dir: &Path,
        stop: &StopToken,
        timeout: Duration,
    ) -> Result<Vec<LiveSession>, AgentsError> {
        if stop.is_stopped() {
            return Err(AgentsError::Aborted);
        }
        let mut cmd = Command::new(&self.exe);
        cmd.args(["agents", "--json"])
            .env("CLAUDE_CONFIG_DIR", profile_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        self.platform.spawn_hidden(&mut cmd);
        let mut child = cmd.spawn().map_err(AgentsError::Spawn)?;
        let mut stdout = child.stdout.take().expect("piped stdout");

        // The reader thread is never joined: after a kill a grandchild may still hold the pipe.
        let (tx, rx) = mpsc::channel::<Reader>();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 64 * 1024];
            loop {
                match stdout.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if buf.len() + n > MAX_AGENTS_STDOUT_BYTES {
                            let _ = tx.send(Reader::TooLarge);
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                }
            }
            let _ = tx.send(Reader::Done(buf));
        });

        let deadline = Instant::now() + timeout;
        let mut output: Option<Vec<u8>> = None;
        let status = loop {
            if stop.is_stopped() {
                self.kill(&mut child);
                return Err(AgentsError::Aborted);
            }
            if Instant::now() >= deadline {
                self.kill(&mut child);
                return Err(AgentsError::TimedOut);
            }
            if output.is_none() {
                match rx.recv_timeout(POLL) {
                    Ok(Reader::Done(b)) => output = Some(b),
                    Ok(Reader::TooLarge) => {
                        self.kill(&mut child);
                        return Err(AgentsError::TooLarge);
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    // The reader ended without a result: treat as end of output.
                    Err(RecvTimeoutError::Disconnected) => output = Some(Vec::new()),
                }
            } else {
                match child.try_wait() {
                    Ok(Some(s)) => break s,
                    Ok(None) => {
                        stop.wait_timeout(POLL);
                    }
                    Err(e) => {
                        self.kill(&mut child);
                        return Err(AgentsError::Spawn(e));
                    }
                }
            }
        };
        if !status.success() {
            return Err(AgentsError::Exited(status.code().unwrap_or(-1)));
        }
        let bytes = output.unwrap_or_default();
        let json: Value = serde_json::from_slice(&bytes).map_err(|_| AgentsError::InvalidJson)?;
        if !json.is_array() {
            return Err(AgentsError::InvalidJson);
        }
        Ok(parse_agents(&json))
    }
}

impl ClaudeCli for RealClaudeCli {
    fn agents(
        &self,
        profile_dir: &Path,
        stop: &StopToken,
    ) -> Result<Vec<LiveSession>, AgentsError> {
        self.agents_with_timeout(profile_dir, stop, Duration::from_millis(AGENTS_TIMEOUT_MS))
    }
}
