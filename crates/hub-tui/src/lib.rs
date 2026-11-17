//! The read-only terminal UI: session list grouped by profile, a Status pane and a History pane
//! with search (ratatui). Port of `src/tui/*`; sending arrives with a later slice.
//!
//! [`App`](app::App) is plain state driven by `handle_key`; [`Driver`] owns the store and the
//! platform and carries out what the keys ask; [`run_loop`] is the event loop over any ratatui
//! backend and any [`Events`] source, and [`run`] wires it to the real terminal.
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyEvent, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use hub_core::platform::{FocusResult, Platform};
use hub_core::store::Store;
use hub_core::view::snapshot::read_snapshot;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::{Frame, Terminal};

pub mod app;
pub mod format;
pub mod history_pane;
pub mod list;
pub mod status_pane;

use app::{Action, App};
use history_pane::{PAGE, SEARCH_LIMIT};

/// How often the snapshot is read again.
pub const REFRESH: Duration = Duration::from_secs(1);

/// The store, the platform and the app state together: what one key or one refresh needs.
pub struct Driver {
    path: PathBuf,
    platform: Arc<dyn Platform>,
    known_profiles: Vec<String>,
    daemon_running: Box<dyn Fn() -> bool>,
    clock: Box<dyn Fn() -> i64>,
    /// None until `hub.db` opens: the daemon creates it, so a missing file is retried every
    /// refresh and shown as an error meanwhile.
    store: Option<Store>,
    app: App,
}

impl Driver {
    /// `clock` is epoch ms; `warning` stays on the last row (the daemon did not start).
    pub fn new(
        store_path: &Path,
        platform: Arc<dyn Platform>,
        known_profiles: &[&str],
        daemon_running: Box<dyn Fn() -> bool>,
        clock: Box<dyn Fn() -> i64>,
        warning: Option<String>,
    ) -> Driver {
        Driver {
            path: store_path.to_path_buf(),
            platform,
            known_profiles: known_profiles.iter().map(|p| (*p).to_string()).collect(),
            daemon_running,
            clock,
            store: None,
            app: App::new(warning),
        }
    }

    pub fn app(&self) -> &App {
        &self.app
    }

    /// Reads a fresh snapshot. An error (no file yet, locked, newer schema) keeps the previous
    /// snapshot on screen and shows the error line; the next refresh tries again.
    pub fn refresh(&mut self) {
        let now = (self.clock)();
        if self.store.is_none() {
            match Store::open_read_only(&self.path) {
                Ok(s) => self.store = Some(s),
                Err(e) => {
                    self.app.on_snapshot(Err(e.to_string()), now);
                    return;
                }
            }
        }
        if let Some(store) = &self.store {
            let known: Vec<&str> = self.known_profiles.iter().map(String::as_str).collect();
            let result = read_snapshot(store, (self.daemon_running)(), &known, now)
                .map_err(|e| e.to_string());
            self.app.on_snapshot(result, now);
        }
        self.pump_history();
    }

    /// Reads the newest History page when the pane is shown and due.
    fn pump_history(&mut self) {
        let Some(sid) = self.app.history_request() else {
            return;
        };
        let Some(store) = &self.store else {
            return;
        };
        match store.history(&sid, PAGE as i64, None) {
            Ok(page) => self.app.history_loaded(&sid, page),
            Err(e) => self.app.pane_error(&e.to_string()),
        }
    }

    /// Handles one key; false when the app should quit.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        match self.app.handle_key(key) {
            Action::None => {}
            Action::Quit => return false,
            Action::Refresh => self.refresh(),
            Action::Focus(pid) => self.focus(pid),
            Action::Search(q) => self.search(q),
            Action::LoadOlder(ts, uuid) => self.load_older(&ts, &uuid),
        }
        self.pump_history();
        true
    }

    pub fn paste(&mut self, text: &str) {
        self.app.paste(text);
    }

    fn focus(&mut self, pid: i64) {
        let text = match u32::try_from(pid) {
            Err(_) => "could not focus: invalid process id".to_string(),
            Ok(pid) => match self.platform.focus_session_window(pid) {
                FocusResult::Focused { title } if title.is_empty() => "focused: window".into(),
                FocusResult::Focused { title } => format!("focused: {title}"),
                FocusResult::NotFound { reason } if reason.is_empty() => {
                    "could not focus: unknown reason".into()
                }
                FocusResult::NotFound { reason } => format!("could not focus: {reason}"),
                FocusResult::Unsupported => "could not focus: not supported on this OS yet".into(),
            },
        };
        self.app.show_toast(&text, Instant::now());
    }

    fn search(&mut self, query: String) {
        let Some(store) = &self.store else { return };
        match store.search(&query, SEARCH_LIMIT) {
            Ok(hits) => self.app.search_done(query, hits),
            Err(e) => self.app.pane_error(&e.to_string()),
        }
    }

    fn load_older(&mut self, ts: &str, uuid: &str) {
        let Some(store) = &self.store else { return };
        let sid = self.app.history_session_id().to_string();
        match store.history(&sid, PAGE as i64, Some((ts, uuid))) {
            Ok(page) => self.app.older_loaded(&sid, page),
            Err(e) => self.app.pane_error(&e.to_string()),
        }
    }

    /// Expires the toast.
    pub fn tick(&mut self, now: Instant) {
        self.app.expire_toast(now);
    }

    pub fn toast_deadline(&self) -> Option<Instant> {
        self.app.toast_deadline()
    }

    pub fn draw(&mut self, f: &mut Frame) {
        self.app.set_area(f.area());
        self.app.render(f);
    }
}

/// Where input events come from: the real terminal, or a script in tests.
pub trait Events {
    /// The next event, or None when `timeout` passes without one.
    fn poll(&mut self, timeout: Duration) -> io::Result<Option<Event>>;
}

pub struct CrosstermEvents;

impl Events for CrosstermEvents {
    fn poll(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        if event::poll(timeout)? {
            Ok(Some(event::read()?))
        } else {
            Ok(None)
        }
    }
}

/// Draws, waits for an event (never past the next refresh or the toast's end) and handles it,
/// until a key quits. Windows reports key releases too; only presses and repeats count.
pub fn run_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    driver: &mut Driver,
    events: &mut dyn Events,
    refresh: Duration,
) -> io::Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    driver.refresh();
    let mut next_refresh = Instant::now() + refresh;
    loop {
        driver.tick(Instant::now());
        terminal
            .draw(|f| driver.draw(f))
            .map_err(io::Error::other)?;
        let now = Instant::now();
        let mut timeout = next_refresh.saturating_duration_since(now);
        if let Some(d) = driver.toast_deadline() {
            timeout = timeout.min(d.saturating_duration_since(now));
        }
        match events.poll(timeout)? {
            Some(Event::Key(k)) if matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                if !driver.key(k) {
                    return Ok(());
                }
            }
            Some(Event::Paste(text)) => driver.paste(&text),
            _ => {}
        }
        if Instant::now() >= next_refresh {
            driver.refresh();
            next_refresh = Instant::now() + refresh;
        }
    }
}

/// True while the terminal is in the TUI's mode; the panic hook restores only then.
static ACTIVE: AtomicBool = AtomicBool::new(false);

fn restore_terminal() {
    if ACTIVE.swap(false, Ordering::SeqCst) {
        let mut out = io::stdout();
        let _ = disable_raw_mode();
        let _ = crossterm::execute!(
            out,
            crossterm::event::DisableBracketedPaste,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
    }
}

/// Raw mode, the alternate screen and bracketed paste, undone when dropped: on every return and
/// unwind. The panic hook undoes it first, so the panic message lands on the normal screen.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<TerminalGuard> {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            previous(info);
        }));
        ACTIVE.store(true, Ordering::SeqCst);
        let guard = TerminalGuard;
        enable_raw_mode()?;
        crossterm::execute!(
            io::stdout(),
            EnterAlternateScreen,
            crossterm::event::EnableBracketedPaste,
            crossterm::cursor::Hide
        )?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |t| i64::try_from(t.as_millis()).unwrap_or(i64::MAX))
}

/// Runs the TUI on the real terminal until `q` or Ctrl-C. `daemon_running` is asked on every
/// refresh. The terminal is restored on every exit path, a panic included.
pub fn run(
    store_path: &Path,
    platform: Arc<dyn Platform>,
    known_profiles: &[&str],
    daemon_running: Box<dyn Fn() -> bool>,
) -> io::Result<()> {
    run_with_warning(store_path, platform, known_profiles, daemon_running, None)
}

/// [`run`] with a line kept on the last row for the whole run.
pub fn run_with_warning(
    store_path: &Path,
    platform: Arc<dyn Platform>,
    known_profiles: &[&str],
    daemon_running: Box<dyn Fn() -> bool>,
    warning: Option<String>,
) -> io::Result<()> {
    let mut driver = Driver::new(
        store_path,
        platform,
        known_profiles,
        daemon_running,
        Box::new(now_ms),
        warning,
    );
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    run_loop(&mut terminal, &mut driver, &mut CrosstermEvents, REFRESH)
}
