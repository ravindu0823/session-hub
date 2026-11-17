//! The real [`Platform`]: Windows (ACL through `icacls`/`whoami`, Win32 process queries, `taskkill`,
//! terminal window focus) and Unix (mode 0700 dirs, process groups). `native()` picks the one for
//! this OS. Port of `src/platform/windows/*` and `src/util/{taskkill,proc}.ts`.
use std::sync::Arc;

use hub_core::platform::Platform;

pub mod exec;
#[cfg(unix)]
pub mod unix;
/// The ACL parsers are portable (tested everywhere); the rest is Windows-only.
pub mod windows;

/// The platform for the OS this runs on.
pub fn native() -> Arc<dyn Platform> {
    #[cfg(windows)]
    {
        Arc::new(windows::WindowsPlatform)
    }
    #[cfg(unix)]
    {
        Arc::new(unix::UnixPlatform)
    }
}

/// Hides a child's console window on Windows (`CREATE_NO_WINDOW`); nothing elsewhere.
pub(crate) fn hide_window(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}
