//! JSON-lines file logger of the daemon (port of `src/daemon/log.ts`), plus a `MemoryLogger` test
//! double.
//!
//! Never pass message text, prompts, bodies or status text. Only counts, timings, ids, names and
//! error names/messages from our own code.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::store::iso_utc_ms;
use crate::text::clip_utf16;

/// One structured field value.
#[derive(Debug, Clone, PartialEq)]
pub enum LogValue {
    Num(f64),
    Bool(bool),
    Str(String),
}

/// Fields of one log line, in order.
pub type LogFields = Vec<(&'static str, LogValue)>;

pub trait Logger: Send + Sync {
    fn info(&self, event: &str, f: LogFields);
    fn warn(&self, event: &str, f: LogFields);
    fn error(&self, event: &str, f: LogFields);
}

/// Rotation size of a log file.
pub const LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;
/// String fields and the event name are cut to this many UTF-16 units (JS `.length`).
const MAX_FIELD_UNITS: usize = 200;
/// The daemon's log file name (`relay.log` is the other one).
pub const DAEMON_LOG: &str = "daemon.log";

/// At most [`MAX_FIELD_UNITS`] UTF-16 units, never splitting a character (JS `slice` could leave half
/// a surrogate pair; this drops the whole character instead).
fn clip(s: &str) -> &str {
    clip_utf16(s, MAX_FIELD_UNITS)
}

/// Fields for a caught error: its name and message, truncated. Rust errors carry no class name
/// (TS logs `e.name`), so `err` is always `Error`; the message says what failed.
pub fn err_fields(e: &dyn std::error::Error) -> LogFields {
    vec![
        ("err", LogValue::Str("Error".to_string())),
        ("msg", LogValue::Str(clip(&e.to_string()).to_string())),
    ]
}

/// `JSON.stringify` of a number: integers without a fraction, `-0` as `0`, NaN and the infinities
/// as `null`.
fn js_number(n: f64) -> String {
    if !n.is_finite() {
        "null".to_string()
    } else if n == 0.0 {
        "0".to_string()
    } else if n.fract() == 0.0 && n.abs() < 1e21 {
        format!("{n:.0}")
    } else {
        // Shortest round-trip form; JS writes large exponents as `1e+21`, serde_json as `1e21`.
        serde_json::to_string(&n).unwrap_or_else(|_| "null".to_string())
    }
}

fn json_value(v: &LogValue) -> String {
    match v {
        LogValue::Num(n) => js_number(*n),
        LogValue::Bool(b) => b.to_string(),
        LogValue::Str(s) => json_string(clip(s)),
    }
}

fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// One log line, as TS builds it: the fields in order, then `ts`, `level` and `event`. Like a JS
/// object, a key set twice keeps its first position and its last value, so a field named `ts`,
/// `level` or `event` is overwritten by the real one.
fn format_line(level: &str, event: &str, fields: &LogFields) -> String {
    let mut out: Vec<(&str, String)> = Vec::with_capacity(fields.len() + 3);
    let mut set = |k: &'static str, v: String| match out.iter_mut().find(|(key, _)| *key == k) {
        Some(slot) => slot.1 = v,
        None => out.push((k, v)),
    };
    for (k, v) in fields {
        set(k, json_value(v));
    }
    set("ts", json_string(&iso_utc_ms(now_ms())));
    set("level", json_string(level));
    set("event", json_string(clip(event)));
    let body: Vec<String> = out
        .iter()
        .map(|(k, v)| format!("{}:{v}", json_string(k)))
        .collect();
    format!("{{{}}}\n", body.join(","))
}

/// JSON lines to one file, rotated at `max_bytes` keeping `.1` and `.2`.
struct FileLogger {
    path: PathBuf,
    max_bytes: u64,
    /// Bytes in the current file, as TS counts them (the size at start plus every line written).
    /// The lock also keeps lines whole and in order across threads.
    size: Mutex<u64>,
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

impl FileLogger {
    fn rotate(&self) -> std::io::Result<()> {
        let one = with_suffix(&self.path, ".1");
        let two = with_suffix(&self.path, ".2");
        let _ = fs::remove_file(&two);
        // No .1 yet is fine.
        let _ = fs::rename(&one, &two);
        fs::rename(&self.path, &one)
    }

    fn append(&self, size: &mut u64, line: &str) -> std::io::Result<()> {
        let bytes = line.len() as u64;
        if *size > 0 && *size + bytes > self.max_bytes {
            self.rotate()?;
            *size = 0;
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?
            .write_all(line.as_bytes())?;
        *size += bytes;
        Ok(())
    }

    fn write(&self, level: &str, event: &str, fields: &LogFields) {
        let line = format_line(level, event, fields);
        let mut size = self.size.lock().unwrap_or_else(|e| e.into_inner());
        // Logging must never break the daemon.
        let _ = self.append(&mut size, &line);
    }
}

impl Logger for FileLogger {
    fn info(&self, event: &str, f: LogFields) {
        self.write("info", event, &f);
    }
    fn warn(&self, event: &str, f: LogFields) {
        self.write("warn", event, &f);
    }
    fn error(&self, event: &str, f: LogFields) {
        self.write("error", event, &f);
    }
}

/// JSON lines to `<dir>/<file>` (the daemon's is [`DAEMON_LOG`]), rotated at `max_bytes`
/// ([`LOG_MAX_BYTES`]) keeping `.1` and `.2`. Creates `dir`. Unlike TS (whose `mkdirSync` throws),
/// a folder that cannot be created is not an error: every write then fails silently, as writes do.
pub fn create_logger(dir: &Path, max_bytes: u64, file: &str) -> Arc<dyn Logger> {
    let _ = fs::create_dir_all(dir);
    let path = dir.join(file);
    let size = fs::metadata(&path).map_or(0, |m| m.len());
    Arc::new(FileLogger {
        path,
        max_bytes,
        size: Mutex::new(size),
    })
}

/// One line captured by [`MemoryLogger`].
#[derive(Debug, Clone, PartialEq)]
pub struct LogEntry {
    pub level: &'static str,
    pub event: String,
    pub fields: LogFields,
}

/// Test double that keeps every line in memory. Public (not `cfg(test)`) so integration tests of
/// this and later modules can use it; production code never constructs it.
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct MemoryLogger {
    entries: Mutex<Vec<LogEntry>>,
}

impl MemoryLogger {
    pub fn new() -> MemoryLogger {
        MemoryLogger::default()
    }

    /// Everything logged so far, oldest first.
    pub fn entries(&self) -> Vec<LogEntry> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn push(&self, level: &'static str, event: &str, fields: LogFields) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(LogEntry {
                level,
                event: event.to_string(),
                fields,
            });
    }
}

impl Logger for MemoryLogger {
    fn info(&self, event: &str, f: LogFields) {
        self.push("info", event, f);
    }
    fn warn(&self, event: &str, f: LogFields) {
        self.push("warn", event, f);
    }
    fn error(&self, event: &str, f: LogFields) {
        self.push("error", event, f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_logger_keeps_every_level_in_order() {
        let log = MemoryLogger::new();
        log.info("a", vec![("n", LogValue::Num(1.0))]);
        log.warn("b", vec![("ok", LogValue::Bool(true))]);
        log.error("c", vec![("s", LogValue::Str("x".into()))]);
        let got: Vec<(&str, String)> = log
            .entries()
            .into_iter()
            .map(|e| (e.level, e.event))
            .collect();
        assert_eq!(
            got,
            [
                ("info", "a".to_string()),
                ("warn", "b".to_string()),
                ("error", "c".to_string())
            ]
        );
    }
}
