//! Unix: a private dir is mode 0700 and owned by the effective user; children run in their own
//! process group so the group can be killed; there is no window focus.
mod privacy;
mod process;
mod signals;

use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use hub_core::discovery::StopToken;
use hub_core::platform::{FocusResult, Platform, SignalGuard};

#[derive(Debug, Default, Clone, Copy)]
pub struct UnixPlatform;

impl Platform for UnixPlatform {
    fn ensure_private_dir(&self, dir: &Path) -> io::Result<()> {
        privacy::ensure_private_dir(dir)
    }

    fn is_private_dir(&self, dir: &Path) -> io::Result<bool> {
        Ok(privacy::is_private_dir(dir))
    }

    fn process_alive(&self, pid: u32) -> bool {
        process::alive(pid)
    }

    fn process_start_time(&self, pid: u32) -> Option<i64> {
        process::start_time(pid)
    }

    fn process_image_path(&self, pid: u32) -> Option<PathBuf> {
        process::image_path(pid)
    }

    /// Its own process group (pgid = its pid), so [`Platform::kill_tree`] reaches its children.
    fn spawn_hidden(&self, cmd: &mut Command) {
        cmd.process_group(0);
    }

    fn kill_tree(&self, pid: u32) -> io::Result<()> {
        process::kill_tree(pid)
    }

    fn focus_session_window(&self, _pid: u32) -> FocusResult {
        FocusResult::Unsupported
    }

    fn stop_on_signals(&self, stop: StopToken) -> io::Result<SignalGuard> {
        signals::stop_on_signals(stop)
    }
}
