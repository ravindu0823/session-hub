//! Ctrl-C (SIGINT) and SIGTERM stop the daemon cleanly instead of ending the process.
//!
//! The handler only bumps an atomic counter (async-signal-safe); a small thread polls it and calls
//! `stop.stop()`. Installed with POSIX `signal(2)` (BSD semantics on glibc and macOS: the handler
//! stays installed and interrupted calls restart). Dropping the guard puts the previous handlers
//! back.
use std::ffi::c_int;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use hub_core::discovery::StopToken;
use hub_core::platform::SignalGuard;
use rustix::process::Signal;

static RECEIVED: AtomicU64 = AtomicU64::new(0);
const POLL: Duration = Duration::from_millis(200);
const SIG_ERR: usize = usize::MAX;

extern "C" fn on_signal(_sig: c_int) {
    RECEIVED.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C" {
    fn signal(signum: c_int, handler: usize) -> usize;
}

fn restore(prev: &[(c_int, usize)]) {
    for &(sig, handler) in prev {
        // SAFETY: puts back the handler `signal` returned for this signal.
        unsafe { signal(sig, handler) };
    }
}

pub fn stop_on_signals(stop: StopToken) -> io::Result<SignalGuard> {
    let seen = RECEIVED.load(Ordering::SeqCst);
    let handler: extern "C" fn(c_int) = on_signal;
    let mut prev = Vec::new();
    for sig in [Signal::INT.as_raw(), Signal::TERM.as_raw()] {
        // SAFETY: the handler only touches an atomic.
        let old = unsafe { signal(sig, handler as usize) };
        if old == SIG_ERR {
            let e = io::Error::last_os_error();
            restore(&prev);
            return Err(e);
        }
        prev.push((sig, old));
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
        restore(&prev);
        done.store(true, Ordering::SeqCst);
        let _ = thread.join();
    }))
}
