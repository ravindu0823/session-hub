//! Watches `<profile>/sessions` for changes. Port of `watchSessions` in `src/discovery/watch.ts`.
//!
//! The watcher only receives file system events; it never opens, reads or stats any file in the
//! folder (so never a `*.key`).
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::config::DEBOUNCE_MS;

#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error("sessions folder is not a directory")]
    NotADirectory,
    #[error("sessions folder cannot be watched: {0}")]
    Notify(#[from] notify::Error),
}

/// True when a change to this filename should trigger a refresh. Unknown (None) names fire.
pub fn should_fire(filename: Option<&str>) -> bool {
    filename.is_none_or(|f| f.ends_with(".json"))
}

/// Stops the watch when dropped; a call still waiting out its debounce is cancelled.
pub struct WatchHandle {
    closed: Arc<AtomicBool>,
    watcher: Option<RecommendedWatcher>,
    thread: Option<JoinHandle<()>>,
    on_drop: Option<Box<dyn FnOnce() + Send>>,
}

impl WatchHandle {
    /// A handle that watches nothing and runs `on_drop` when dropped: the stand-in a fake watch
    /// (tests) returns.
    pub fn from_drop(on_drop: impl FnOnce() + Send + 'static) -> WatchHandle {
        WatchHandle {
            closed: Arc::new(AtomicBool::new(false)),
            watcher: None,
            thread: None,
            on_drop: Some(Box::new(on_drop)),
        }
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        if let Some(f) = self.on_drop.take() {
            f();
        }
        self.closed.store(true, Ordering::SeqCst);
        // Dropping the watcher drops its event sender, which ends the debounce thread.
        self.watcher.take();
        if let Some(t) = self.thread.take()
            && t.thread().id() != std::thread::current().id()
        {
            let _ = t.join();
        }
    }
}

pub fn watch_sessions(
    profile_dir: &Path,
    on_change: impl Fn() + Send + 'static,
) -> Result<WatchHandle, WatchError> {
    watch_sessions_with(profile_dir, on_change, |_| {})
}

/// Like [`watch_sessions`]; `on_error` gets watcher errors and the message of a panicking `on_change`.
pub fn watch_sessions_with(
    profile_dir: &Path,
    on_change: impl Fn() + Send + 'static,
    on_error: impl Fn(String) + Send + Sync + 'static,
) -> Result<WatchHandle, WatchError> {
    let dir = profile_dir.join("sessions");
    // Only the folder itself is looked at, never a file in it.
    if !dir.is_dir() {
        return Err(WatchError::NotADirectory);
    }
    let on_error = Arc::new(on_error);
    let closed = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<()>();

    let handler_closed = closed.clone();
    let handler_error = on_error.clone();
    let handler_tx: Sender<()> = tx;
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if handler_closed.load(Ordering::SeqCst) {
            return;
        }
        match res {
            Err(e) => handler_error(e.to_string()),
            Ok(ev) => {
                // Reads and closes by other programs change nothing.
                if matches!(ev.kind, EventKind::Access(_)) {
                    return;
                }
                let fire = if ev.paths.is_empty() {
                    should_fire(None)
                } else {
                    ev.paths.iter().any(|p| {
                        let name = p.file_name().map(|n| n.to_string_lossy());
                        should_fire(name.as_deref())
                    })
                };
                if fire {
                    let _ = handler_tx.send(());
                }
            }
        }
    })?;
    watcher.watch(&dir, RecursiveMode::NonRecursive)?;

    let thread_closed = closed.clone();
    let thread = std::thread::spawn(move || {
        let debounce = Duration::from_millis(DEBOUNCE_MS);
        // Wait for a first event, then until a full quiet period passes.
        loop {
            // Wakes now and then so a drop is noticed even if the watcher keeps its sender alive.
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(()) => {}
                Err(RecvTimeoutError::Timeout) => {
                    if thread_closed.load(Ordering::SeqCst) {
                        return;
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
            loop {
                match rx.recv_timeout(debounce) {
                    Ok(()) => {}
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            if thread_closed.load(Ordering::SeqCst) {
                return;
            }
            // Fixed text: the panic payload may quote data the callback held, and it would reach
            // daemon.log through `watch_error`.
            if catch_unwind(AssertUnwindSafe(&on_change)).is_err() {
                on_error("on_change panicked".to_string());
            }
        }
    });

    Ok(WatchHandle {
        closed,
        watcher: Some(watcher),
        thread: Some(thread),
        on_drop: None,
    })
}
