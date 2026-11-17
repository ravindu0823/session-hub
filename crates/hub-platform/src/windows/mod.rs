//! Windows: ACL through `icacls`/`whoami`, process queries through Win32, tree kill through
//! `taskkill /T`, terminal window focus. Only the ACL parsers compile elsewhere (they are tested on
//! every OS).
pub mod acl;
#[cfg(windows)]
mod focus;
#[cfg(windows)]
mod process;
#[cfg(windows)]
mod signals;

#[cfg(windows)]
pub use real::WindowsPlatform;

#[cfg(windows)]
mod real {
    use std::ffi::OsString;
    use std::io;
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::Duration;

    use hub_core::discovery::StopToken;
    use hub_core::platform::{FocusResult, Platform, SignalGuard};
    use hub_core::sysbin;
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};

    use super::{acl, focus, process, signals};

    /// `taskkill` still running after this is killed and the tree kill reports failure (TS
    /// `TASKKILL_BOUND_MS`).
    const TASKKILL_BOUND: Duration = Duration::from_millis(2000);

    #[derive(Debug, Default, Clone, Copy)]
    pub struct WindowsPlatform;

    impl Platform for WindowsPlatform {
        fn ensure_private_dir(&self, dir: &Path) -> io::Result<()> {
            acl::ensure_private_dir(dir)
        }

        fn is_private_dir(&self, dir: &Path) -> io::Result<bool> {
            acl::is_private_dir(dir)
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

        fn spawn_hidden(&self, cmd: &mut Command) {
            cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
        }

        /// `taskkill /PID <pid> /T /F` (the process and its children), bounded to 2 s. Port of
        /// `src/util/taskkill.ts`; the caller also kills the root directly. Refuses pid 0 (idle),
        /// 4 (System) and our own pid.
        fn kill_tree(&self, pid: u32) -> io::Result<()> {
            if hub_core::platform::is_dangerous_kill_pid(pid) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("kill_tree refuses pid {pid}"),
                ));
            }
            let argv: Vec<OsString> = vec![
                sysbin::taskkill().into_os_string(),
                "/PID".into(),
                pid.to_string().into(),
                "/T".into(),
                "/F".into(),
            ];
            let r = crate::exec::run(&argv, &[], TASKKILL_BOUND)?;
            if r.code == 0 {
                Ok(())
            } else {
                Err(io::Error::other(format!(
                    "taskkill failed with exit code {}",
                    r.code
                )))
            }
        }

        fn focus_session_window(&self, pid: u32) -> FocusResult {
            focus::focus_session_window(pid)
        }

        fn stop_on_signals(&self, stop: StopToken) -> io::Result<SignalGuard> {
            signals::stop_on_signals(stop)
        }
    }
}
