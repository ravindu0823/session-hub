//! Ctrl-C, Ctrl-Break and closing the console stop the daemon cleanly instead of ending the
//! process. The console handler runs on a thread Windows creates; it bumps a counter that a small
//! thread turns into `stop.stop()`. On close the process ends when the handler returns, so it waits
//! (up to 4.5 s) for the daemon to finish and the guard to drop. Log-off and shutdown events are
//! handled the same way, but Windows does not send them to a process that loads user32.dll, which
//! `hub.exe` does, so they are not relied on. A windowless `hubw.exe` has no console and gets none
//! of these events.
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hub_core::discovery::StopToken;
use hub_core::platform::SignalGuard;
use windows_sys::Win32::System::Console::{
    CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    SetConsoleCtrlHandler,
};
use windows_sys::core::BOOL;

static RECEIVED: AtomicU64 = AtomicU64::new(0);
/// Set when the guard drops: the daemon has stopped.
static DONE: AtomicBool = AtomicBool::new(true);
const POLL: Duration = Duration::from_millis(200);
const CLOSE_WAIT: Duration = Duration::from_millis(4_500);

pub(crate) unsafe extern "system" fn on_ctrl(ctrl: u32) -> BOOL {
    match ctrl {
        CTRL_C_EVENT | CTRL_BREAK_EVENT => {
            RECEIVED.fetch_add(1, Ordering::SeqCst);
            1
        }
        CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => {
            RECEIVED.fetch_add(1, Ordering::SeqCst);
            let end = Instant::now() + CLOSE_WAIT;
            while !DONE.load(Ordering::SeqCst) && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(25));
            }
            1
        }
        _ => 0,
    }
}

pub fn stop_on_signals(stop: StopToken) -> io::Result<SignalGuard> {
    let seen = RECEIVED.load(Ordering::SeqCst);
    DONE.store(false, Ordering::SeqCst);
    // SAFETY: registers a handler with the signature the API expects.
    if unsafe { SetConsoleCtrlHandler(Some(on_ctrl), 1) } == 0 {
        DONE.store(true, Ordering::SeqCst);
        return Err(io::Error::last_os_error());
    }
    let done = Arc::new(AtomicBool::new(false));
    let thread_done = done.clone();
    let thread = std::thread::spawn(move || {
        while !thread_done.load(Ordering::SeqCst) {
            if RECEIVED.load(Ordering::SeqCst) != seen {
                stop.stop();
                return;
            }
            if stop.wait_timeout(POLL) {
                return;
            }
        }
    });
    Ok(SignalGuard::new(move || {
        // SAFETY: removes the handler registered above.
        unsafe { SetConsoleCtrlHandler(Some(on_ctrl), 0) };
        done.store(true, Ordering::SeqCst);
        let _ = thread.join();
        DONE.store(true, Ordering::SeqCst);
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_break_stops_the_token_and_the_guard_removes_the_handler() {
        let stop = StopToken::new();
        let guard = stop_on_signals(stop.clone()).unwrap();
        // SAFETY: calls the handler directly, as the console would.
        assert_eq!(unsafe { on_ctrl(CTRL_BREAK_EVENT) }, 1);
        assert!(stop.wait_timeout(Duration::from_secs(3)));
        drop(guard);
        assert!(DONE.load(Ordering::SeqCst));
        // SAFETY: an unknown event is declined.
        assert_eq!(unsafe { on_ctrl(99) }, 0);
    }
}
