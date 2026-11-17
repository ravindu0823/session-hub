//! Brings the terminal window of a session to the front. Port of `src/platform/windows/focus.ts`
//! without PowerShell: the parent walk uses sysinfo and the start times from `process.rs`, and a
//! process's main window is chosen as .NET's `Process.MainWindowHandle` does (the first visible,
//! unowned top-level window of that pid). It only focuses: it never sends keys or input.
use std::collections::HashSet;

use hub_core::platform::FocusResult;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use windows_sys::Win32::Foundation::{HWND, LPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GetWindow, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, SW_RESTORE, SetForegroundWindow,
    ShowWindow,
};

use super::process;

fn not_found(reason: &str) -> FocusResult {
    FocusResult::NotFound {
        reason: reason.to_string(),
    }
}

/// EnumWindows callback: collects (pid, hwnd) of visible, unowned top-level windows in z-order.
unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> i32 {
    // SAFETY: `lparam` is the `&mut Vec` passed to EnumWindows below, alive for the whole call.
    let out = unsafe { &mut *(lparam as *mut Vec<(u32, HWND)>) };
    // SAFETY: plain Win32 calls on a window handle EnumWindows gave.
    unsafe {
        if GetWindow(hwnd, GW_OWNER).is_null() && IsWindowVisible(hwnd) != 0 {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut pid);
            out.push((pid, hwnd));
        }
    }
    1
}

fn main_windows() -> Vec<(u32, HWND)> {
    let mut out: Vec<(u32, HWND)> = Vec::new();
    // SAFETY: the callback only touches `out` through the pointer, during this call.
    unsafe { EnumWindows(Some(collect), &mut out as *mut Vec<(u32, HWND)> as LPARAM) };
    out
}

fn window_title(hwnd: HWND) -> String {
    // SAFETY: plain Win32 calls; the buffer holds `len + 1` units.
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
}

/// The main window of `pid` or of its nearest ancestor that has one; Err when the process list
/// cannot be read. The walk stops at a loop, at a missing parent, and when a parent is younger than
/// its child (the parent pid was reused) or either start time is unknown.
fn find_window(pid: u32) -> Result<Option<HWND>, ()> {
    let mut sys = System::new().map_err(|_| ())?;
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    let windows = main_windows();
    let mut seen = HashSet::new();
    let mut cur = pid;
    while seen.insert(cur) {
        let Some(proc_) = sys.process(Pid::from_u32(cur)) else {
            break;
        };
        if let Some((_, hwnd)) = windows.iter().find(|(p, _)| *p == cur) {
            return Ok(Some(*hwnd));
        }
        let Some(parent) = proc_.parent().map(Pid::as_u32) else {
            break;
        };
        if parent == 0 {
            break;
        }
        match (process::start_time(parent), process::start_time(cur)) {
            (Some(p), Some(c)) if p <= c => cur = parent,
            _ => break,
        }
    }
    Ok(None)
}

pub fn focus_session_window(pid: u32) -> FocusResult {
    if pid == 0 {
        return not_found("error");
    }
    let hwnd = match find_window(pid) {
        Ok(Some(h)) => h,
        Ok(None) => return not_found("no-window"),
        Err(()) => return not_found("error"),
    };
    let title = window_title(hwnd);
    // SAFETY: plain Win32 calls on a window handle; a stale handle makes them fail, not misbehave.
    unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        if SetForegroundWindow(hwnd) == 0 {
            return not_found("focus-denied");
        }
    }
    FocusResult::Focused { title }
}
