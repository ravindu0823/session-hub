//! Unix: SIGTERM and SIGINT stop the token while the guard lives. One test in its own binary: a
//! signal sent while no handler is installed would end the test process.
#![cfg(unix)]

use std::time::Duration;

use hub_core::discovery::StopToken;
use rustix::process::{Signal, getpid, kill_process};

#[test]
fn sigterm_and_sigint_stop_the_token_while_the_guard_lives() {
    let platform = hub_platform::native();
    for sig in [Signal::TERM, Signal::INT] {
        let stop = StopToken::new();
        let guard = platform.stop_on_signals(stop.clone()).unwrap();
        assert!(!stop.wait_timeout(Duration::from_millis(50)));
        kill_process(getpid(), sig).unwrap();
        assert!(stop.wait_timeout(Duration::from_secs(3)), "{sig:?}");
        drop(guard);
    }
}
