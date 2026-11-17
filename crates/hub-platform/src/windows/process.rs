//! Process queries through one `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` handle, closed on
//! every path. Port of `src/platform/windows/process.ts`; liveness is libuv's `kill(pid, 0)`.
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, FILETIME, GetLastError, HANDLE, STILL_ACTIVE,
};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};

/// Longest path QueryFullProcessImageNameW can return, in UTF-16 units.
const MAX_PATH_UNITS: u32 = 32_768;
/// 100 ns FILETIME ticks between 1601-01-01 and 1970-01-01.
const FILETIME_UNIX_EPOCH: u64 = 116_444_736_000_000_000;

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: a handle OpenProcess returned, closed exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

/// A query handle, or the Win32 error OpenProcess gave (pid 0 is refused without a call).
fn open(pid: u32) -> Result<Handle, u32> {
    if pid == 0 {
        return Err(0);
    }
    // SAFETY: plain Win32 call; a null result is checked.
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        // SAFETY: reads this thread's last error right after the failed call.
        Err(unsafe { GetLastError() })
    } else {
        Ok(Handle(h))
    }
}

/// False once the process has exited (its object can outlive it while anyone holds a handle).
fn running(h: &Handle) -> bool {
    let mut code = 0u32;
    // SAFETY: valid handle and out pointer.
    let ok = unsafe { GetExitCodeProcess(h.0, &mut code) } != 0;
    !ok || code == STILL_ACTIVE as u32
}

/// A query handle to a process that is still running.
fn open_running(pid: u32) -> Option<Handle> {
    open(pid).ok().filter(running)
}

/// True while the process runs; also when it exists but may not be opened (access denied).
pub fn alive(pid: u32) -> bool {
    match open(pid) {
        Err(e) => e == ERROR_ACCESS_DENIED,
        Ok(h) => running(&h),
    }
}

/// Full Win32 path of the executable; None when the pid is invalid, gone (also exited but still
/// held open) or cannot be opened.
pub fn image_path(pid: u32) -> Option<PathBuf> {
    let h = open_running(pid)?;
    let mut buf = vec![0u16; MAX_PATH_UNITS as usize];
    let mut size = MAX_PATH_UNITS;
    // SAFETY: the buffer holds `size` units; `size` is updated to the length written.
    if unsafe { QueryFullProcessImageNameW(h.0, 0, buf.as_mut_ptr(), &mut size) } == 0 {
        return None;
    }
    let len = (size as usize).min(buf.len());
    Some(PathBuf::from(OsString::from_wide(&buf[..len])))
}

/// GetProcessTimes creation time in epoch ms; None when the pid is invalid, gone (also exited but
/// still held open) or cannot be opened. With the pid it names one process: a pid reused later has a later start time.
pub fn start_time(pid: u32) -> Option<i64> {
    let h = open_running(pid)?;
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: valid handle and four out pointers.
    let ok = unsafe { GetProcessTimes(h.0, &mut created, &mut exited, &mut kernel, &mut user) };
    if ok == 0 {
        return None;
    }
    let ticks = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    if ticks <= FILETIME_UNIX_EPOCH {
        return None;
    }
    i64::try_from((ticks - FILETIME_UNIX_EPOCH) / 10_000).ok()
}
