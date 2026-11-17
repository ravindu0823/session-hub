//! Runs a short-lived tool with an argv array and a timeout. Port of
//! `src/platform/windows/exec.ts`.
use std::ffi::OsString;
use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Stdout kept beyond this is dropped, so a runaway child cannot grow memory without bound.
const MAX_STDOUT_BYTES: usize = 1024 * 1024;
const POLL: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecResult {
    /// Exit code; -1 when there is none (killed by a signal).
    pub code: i32,
    pub stdout: String,
}

/// Runs `argv` (stdin and stderr ignored, no console window, `env` added to ours). Never fails on a
/// non-zero exit; the caller decides. A child still running at `timeout` is killed and gives a
/// non-zero code. Errors only when it cannot be started.
pub fn run(argv: &[OsString], env: &[(&str, &str)], timeout: Duration) -> io::Result<ExecResult> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty argv"))?;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    crate::hide_window(&mut cmd);
    let deadline = Instant::now() + timeout;
    let mut child = cmd.spawn()?;

    // Read on a thread into a shared buffer: a grandchild holding the pipe cannot block us.
    let out = Arc::new(Mutex::new(Vec::<u8>::new()));
    let mut stdout = child.stdout.take().expect("piped stdout");
    let sink = Arc::clone(&out);
    let reader = std::thread::spawn(move || {
        let mut chunk = [0u8; 16 * 1024];
        while let Ok(n) = stdout.read(&mut chunk) {
            if n == 0 {
                break;
            }
            let mut buf = sink.lock().unwrap_or_else(|e| e.into_inner());
            let room = MAX_STDOUT_BYTES.saturating_sub(buf.len());
            buf.extend_from_slice(&chunk[..n.min(room)]);
        }
    });

    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(POLL);
    };
    let code = match status {
        Some(s) => {
            // Exited in time: wait for the pipe to drain, but never past the deadline.
            while !reader.is_finished() && Instant::now() < deadline {
                std::thread::sleep(POLL);
            }
            s.code().unwrap_or(-1)
        }
        None => -1,
    };
    let bytes = out.lock().unwrap_or_else(|e| e.into_inner()).clone();
    Ok(ExecResult {
        code,
        stdout: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

/// The tool's base name without `.exe`, for messages: tools run by absolute path.
pub fn tool_name(path: &Path) -> String {
    let s = path.to_string_lossy();
    let base = s.rsplit(['\\', '/']).next().unwrap_or("");
    match base.len().checked_sub(4) {
        Some(i) if base.is_char_boundary(i) && base[i..].eq_ignore_ascii_case(".exe") => {
            base[..i].to_string()
        }
        _ => base.to_string(),
    }
}

/// [`run`] with the default timeout; a non-zero exit is an error naming only the tool and the code.
pub fn run_checked(argv: &[OsString]) -> io::Result<ExecResult> {
    let r = run(argv, &[], DEFAULT_TIMEOUT)?;
    if r.code != 0 {
        let tool = argv
            .first()
            .map(|p| tool_name(Path::new(p)))
            .unwrap_or_default();
        return Err(io::Error::other(format!(
            "{tool} failed with exit code {}",
            r.code
        )));
    }
    Ok(r)
}
