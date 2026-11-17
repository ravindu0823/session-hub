//! Argument dispatch and the real dependency wiring. Exit codes: 0 ok, 1 failure, 2 usage,
//! 3 stopped.
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use hub_core::config::{Config, current_os, resolve_config};
use hub_core::discovery::{ClaudeCli, RealClaudeCli, find_claude};
use hub_core::platform::Platform;

use crate::daemon_cmd;

pub const USAGE: &str = "Usage:
  hub                     open the terminal UI (starts the daemon if needed)
  hub daemon run          run the daemon in this console
  hub daemon start        start the daemon hidden in the background
  hub daemon stop         stop the daemon
  hub daemon status       RUNNING pid=<n> (exit 0) or STOPPED (exit 3)
  hub --version           print the version
  hub --help              print this help";

/// Everything the commands touch outside their own logic.
pub struct CliDeps {
    /// An error is reported by the commands that need the config (not by `--version`/`--help`).
    pub config: Result<Config, String>,
    pub platform: Arc<dyn Platform>,
    pub cli: Arc<dyn ClaudeCli>,
    /// This executable; the daemon is started from it (or from `hubw` next to it on Windows).
    pub exe: PathBuf,
    pub out: Box<dyn Fn(&str) + Send + Sync>,
    pub err: Box<dyn Fn(&str) + Send + Sync>,
}

/// Writes ignore errors: `hubw` has no console, and a closed pipe must not panic.
fn write_line(mut w: impl std::io::Write, text: &str) {
    let _ = writeln!(w, "{text}");
}

/// The wiring of `hub` and `hubw`: environment, the native platform, the real `claude` CLI.
pub fn real_deps() -> CliDeps {
    let platform = hub_platform::native();
    let config =
        resolve_config(&|name| std::env::var(name).ok(), current_os()).map_err(|e| e.to_string());
    let claude = std::env::var_os("PATH")
        .and_then(|p| find_claude(&p))
        .unwrap_or_else(|| PathBuf::from("claude"));
    CliDeps {
        config,
        cli: Arc::new(RealClaudeCli {
            exe: claude,
            platform: platform.clone(),
        }),
        platform,
        exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("hub")),
        out: Box::new(|t| write_line(std::io::stdout().lock(), t)),
        err: Box::new(|t| write_line(std::io::stderr().lock(), t)),
    }
}

/// `hub` with no arguments (`cmdTui`): the data folder, the daemon (started unless it runs; a start
/// that fails is a warning on the last row, not an error), then the TUI.
fn tui(d: &CliDeps) -> i32 {
    let config = match d.config.as_ref() {
        Ok(c) => c,
        Err(e) => {
            (d.err)(&format!("hub: {e}"));
            return 1;
        }
    };
    if !config.data_dir.exists()
        && let Err(e) = d.platform.ensure_private_dir(&config.data_dir)
    {
        (d.err)(&format!(
            "hub: cannot create {}: {e}",
            config.data_dir.display()
        ));
        return 1;
    }
    let run_dir = config.run_dir();
    let warning = match daemon_cmd::ensure_started(d, &run_dir) {
        daemon_cmd::Started::Already(_) | daemon_cmd::Started::Started(_) => None,
        daemon_cmd::Started::SpawnFailed(_) | daemon_cmd::Started::TimedOut => {
            Some(daemon_cmd::start_warning(&run_dir))
        }
    };
    let profiles: Vec<&str> = config.profiles.iter().map(|p| p.id.as_str()).collect();
    let result = hub_tui::run_with_warning(
        &config.db_path(),
        d.platform.clone(),
        &profiles,
        daemon_cmd::daemon_running_probe(d, run_dir),
        warning,
    );
    match result {
        Ok(()) => 0,
        Err(e) => {
            (d.err)(&format!("hub: {e}"));
            1
        }
    }
}

/// Runs one command; returns the process exit code.
pub fn run(args: Vec<OsString>, deps: &CliDeps) -> i32 {
    let args: Vec<String> = args
        .into_iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let rest: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();
    let usage = || {
        (deps.err)(USAGE);
        2
    };
    match args.first().map(String::as_str) {
        None => tui(deps),
        Some("--version" | "-v") if rest.is_empty() => {
            (deps.out)(&format!("hub {}", env!("CARGO_PKG_VERSION")));
            0
        }
        Some("--help" | "-h" | "help") if rest.is_empty() => {
            (deps.out)(USAGE);
            0
        }
        Some("daemon") => match rest.as_slice() {
            ["status"] => daemon_cmd::status(deps),
            ["start"] => daemon_cmd::start(deps),
            ["stop"] => daemon_cmd::stop(deps),
            ["run"] => daemon_cmd::run(deps),
            _ => usage(),
        },
        Some(_) => usage(),
    }
}

/// `hubw` (no console window): like [`run`], except that no arguments is a usage error with no
/// output, because a TUI or a message nobody can see is no use. Returns the exit code.
pub fn run_windowless(args: Vec<OsString>, deps: &CliDeps) -> i32 {
    if args.is_empty() {
        return 2;
    }
    run(args, deps)
}
