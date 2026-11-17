//! `hubw.exe`: `hub` without a console window. `hub daemon start` and the autostart run
//! `hubw.exe daemon run`. With no arguments it prints nothing and exits 2 (no TUI without a
//! console). Not a Windows build: only says so.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    let args = std::env::args_os().skip(1).collect();
    let deps = hub::cli::real_deps();
    std::process::ExitCode::from(hub::cli::run_windowless(args, &deps) as u8)
}

#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    eprintln!("hubw is the Windows build of hub; run hub");
    std::process::ExitCode::from(2)
}
