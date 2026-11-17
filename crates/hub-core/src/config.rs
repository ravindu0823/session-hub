//! Paths and constants. Pure: paths come from an injected env lookup so tests never touch real folders.
use std::fmt;
use std::path::PathBuf;

pub const DEBOUNCE_MS: u64 = 500;
pub const SAFETY_POLL_MS: u64 = 30_000;
pub const AGENTS_TIMEOUT_MS: u64 = 10_000;
pub const RETENTION_MS: u64 = 7 * 86_400_000;
pub const FIRST_INDEX_BYTES: u64 = 20 * 1024 * 1024;
pub const DELIVERY_WINDOW_MS: u64 = 30 * 60_000;
/// The daemon expires overdue sends this often.
pub const EXPIRE_EVERY_MS: u64 = 30_000;
pub const RELAY_NAME: &str = "session-hub-relay";
/// Name prefix of the throwaway sessions the relay eval starts: relays to them are expected.
pub const EVAL_SESSION_PREFIX: &str = "sh-eval-";
/// How long after the last stray relay message the TUI and the tray keep warning about it.
pub const STRAY_WARN_MS: u64 = 24 * 3_600_000;
/// Per-relay trace (outcome, timing, cost, tokens, model; never text), in the run folder.
pub const RELAY_LOG: &str = "relay.log";
/// In the run folder: the daemon was stopped on purpose, so the tray does not start it again.
pub const DAEMON_STOPPED_FILE: &str = "daemon.stopped";

pub const MESSAGE_HEADER: &str = "[Session Hub: message from the owner. Reply in your normal terminal reply; do not SendMessage back, the sender has exited.]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Windows,
    MacOs,
    Linux,
}

pub fn current_os() -> Os {
    if cfg!(windows) {
        Os::Windows
    } else if cfg!(target_os = "macos") {
        Os::MacOs
    } else {
        Os::Linux
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileId {
    Claude,
    ClaudePersonal,
}

impl ProfileId {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProfileId::Claude => "claude",
            ProfileId::ClaudePersonal => "claude-personal",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub id: ProfileId,
    pub dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub profiles: Vec<Profile>,
    pub data_dir: PathBuf,
}

impl Config {
    /// The separator follows the data dir's own style, so a Windows-style dir built on Unix (and the
    /// reverse) still gets its own separator; `Path::join` would use the host's.
    fn child(&self, name: &str) -> PathBuf {
        match self.data_dir.to_str() {
            Some(d) => {
                let sep = if d.contains('\\') && !d.contains('/') {
                    '\\'
                } else {
                    '/'
                };
                PathBuf::from(join(d, sep, name))
            }
            None => self.data_dir.join(name),
        }
    }
    pub fn run_dir(&self) -> PathBuf {
        self.child("run")
    }
    pub fn relay_dir(&self) -> PathBuf {
        self.child("relay")
    }
    pub fn db_path(&self) -> PathBuf {
        self.child("hub.db")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

fn required(env: &dyn Fn(&str) -> Option<String>, name: &str) -> Result<String, ConfigError> {
    match env(name) {
        Some(v) if !v.is_empty() => Ok(v),
        _ => Err(ConfigError(format!("{name} is not set"))),
    }
}

fn join(base: &str, sep: char, rest: &str) -> String {
    if base.ends_with(sep) {
        format!("{base}{rest}")
    } else {
        format!("{base}{sep}{rest}")
    }
}

/// Only `~/.claude` and `~/.claude-personal` are ever listed (never `.tclaude` or any other profile).
pub fn resolve_config(env: &dyn Fn(&str) -> Option<String>, os: Os) -> Result<Config, ConfigError> {
    let (sep, home, data) = match os {
        Os::Windows => {
            let home = required(env, "USERPROFILE")?;
            let local = required(env, "LOCALAPPDATA")?;
            ('\\', home, join(&local, '\\', "SessionHub"))
        }
        Os::MacOs => {
            let home = required(env, "HOME")?;
            let data = join(&home, '/', "Library/Application Support/SessionHub");
            ('/', home, data)
        }
        Os::Linux => {
            let home = required(env, "HOME")?;
            let data = match env("XDG_DATA_HOME") {
                Some(x) if x.starts_with('/') => join(&x, '/', "session-hub"),
                _ => join(&home, '/', ".local/share/session-hub"),
            };
            ('/', home, data)
        }
    };
    let profile = |id, name: &str| Profile {
        id,
        dir: PathBuf::from(join(&home, sep, name)),
    };
    Ok(Config {
        profiles: vec![
            profile(ProfileId::Claude, ".claude"),
            profile(ProfileId::ClaudePersonal, ".claude-personal"),
        ],
        data_dir: PathBuf::from(data),
    })
}
