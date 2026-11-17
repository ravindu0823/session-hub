//! The app state (`src/tui/App.tsx`): a plain struct driven by [`App::handle_key`], so it is
//! testable without a terminal. It never touches the store; keys that need it return an [`Action`]
//! and the [`crate::Driver`] carries it out.
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hub_core::security::clean_text;
use hub_core::store::{MessageRow, SearchHit, SessionView};
use hub_core::view::snapshot::HubSnapshot;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span};
use ratatui::widgets::Paragraph;

use crate::format::session_label;
use crate::history_pane::{HistoryState, Outcome};
use crate::list::{Group, group_sessions, list_lines, truncate_end};
use crate::status_pane::{Line, max_offset, status_lines, visible};

pub const TOAST: Duration = Duration::from_secs(3);
pub const SEND_LINE: &str = "Sending arrives in the next version of the Rust hub.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Status,
    History,
    Send,
}

/// What the driver does after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    /// Read the snapshot now (`r`).
    Refresh,
    /// Bring the window of this pid to the front (`j`).
    Focus(i64),
    /// Run this search (Enter in the search box).
    Search(String),
    /// Load the page before `(ts, uuid)` of the selected session's History.
    LoadOlder(String, String),
}

/// Where each part sits in a terminal `area`.
struct Areas {
    header: Rect,
    list: Rect,
    border: Rect,
    right: Rect,
    footer: Rect,
    warning: Option<Rect>,
}

fn areas(area: Rect, has_warning: bool) -> Areas {
    let row = |y: u16| {
        Rect::new(
            area.x,
            area.y + y.min(area.height.saturating_sub(1)),
            area.width,
            1,
        )
    };
    let tail = u16::from(has_warning);
    let body_h = area.height.saturating_sub(2 + tail).max(1);
    let body_y = area.y + 1;
    let left_w = ((f64::from(area.width) * 0.3).floor() as u16)
        .max(22)
        .min(area.width);
    let list_w = left_w.saturating_sub(1);
    let right_x = (area.x + left_w + 1).min(area.x + area.width);
    Areas {
        header: row(0),
        list: Rect::new(area.x, body_y, list_w, body_h),
        border: Rect::new(
            area.x + list_w,
            body_y,
            left_w.saturating_sub(list_w),
            body_h,
        ),
        right: Rect::new(
            right_x,
            body_y,
            (area.x + area.width).saturating_sub(right_x),
            body_h,
        ),
        footer: row(1 + body_h),
        warning: has_warning.then(|| row(2 + body_h)),
    }
}

pub struct App {
    snapshot: Option<HubSnapshot>,
    store_error: Option<String>,
    pane_error: Option<String>,
    tab: Tab,
    selected_id: Option<String>,
    toast: Option<(String, Instant)>,
    status_offset: usize,
    status_sid: String,
    history: HistoryState,
    history_stale: bool,
    warning: Option<String>,
    area: Rect,
    now_ms: i64,
}

impl App {
    /// `warning` stays on the last row for the whole run (the daemon did not start).
    pub fn new(warning: Option<String>) -> App {
        App {
            snapshot: None,
            store_error: None,
            pane_error: None,
            tab: Tab::Status,
            selected_id: None,
            toast: None,
            status_offset: 0,
            status_sid: String::new(),
            history: HistoryState::new(""),
            history_stale: false,
            warning: warning.map(|w| clean_text(&w)),
            area: Rect::new(0, 0, 80, 24),
            now_ms: 0,
        }
    }

    pub fn tab(&self) -> Tab {
        self.tab
    }

    pub fn snapshot(&self) -> Option<&HubSnapshot> {
        self.snapshot.as_ref()
    }

    /// The error line's text: the store's, else the panes'.
    pub fn error(&self) -> Option<&str> {
        self.store_error.as_deref().or(self.pane_error.as_deref())
    }

    /// The terminal area the next keys are interpreted in (pane sizes).
    pub fn set_area(&mut self, area: Rect) {
        self.area = area;
    }

    /// A snapshot read: a good one replaces the previous and clears the errors; an error keeps the
    /// previous snapshot (never shown as "no sessions") and is shown on the error line.
    pub fn on_snapshot(&mut self, result: Result<HubSnapshot, String>, now_ms: i64) {
        self.now_ms = now_ms;
        match result {
            Ok(s) => {
                self.snapshot = Some(s);
                self.store_error = None;
                self.pane_error = None;
            }
            Err(e) => self.store_error = Some(clean_text(&e)),
        }
        self.history_stale = true;
        self.sync_selection();
    }

    pub fn history_session_id(&self) -> &str {
        &self.history.session_id
    }

    pub fn pane_error(&mut self, message: &str) {
        self.pane_error = Some(clean_text(message));
    }

    pub fn show_toast(&mut self, text: &str, now: Instant) {
        self.toast = Some((clean_text(text), now + TOAST));
    }

    pub fn expire_toast(&mut self, now: Instant) {
        if self.toast.as_ref().is_some_and(|(_, until)| now >= *until) {
            self.toast = None;
        }
    }

    pub fn toast_deadline(&self) -> Option<Instant> {
        self.toast.as_ref().map(|(_, until)| *until)
    }

    fn sessions(&self) -> &[SessionView] {
        self.snapshot.as_ref().map_or(&[], |s| &s.sessions)
    }

    fn groups(&self) -> Vec<Group<'_>> {
        group_sessions(self.sessions())
    }

    /// The selected session: the one named by `selected_id`, else the first listed.
    fn selected(&self) -> Option<&SessionView> {
        let groups = self.groups();
        let flat: Vec<&SessionView> = groups
            .iter()
            .flat_map(|g| g.sessions.iter().copied())
            .collect();
        let hit = self
            .selected_id
            .as_deref()
            .and_then(|id| flat.iter().find(|s| s.session_id == id));
        hit.or_else(|| flat.first()).copied()
    }

    fn selected_pid(&self) -> Option<(i64, bool)> {
        self.selected().map(|s| (s.pid, s.ended_at.is_some()))
    }

    /// Each session has its own History and Status scroll: a different selection starts fresh.
    fn sync_selection(&mut self) {
        let Some(id) = self.selected().map(|s| s.session_id.clone()) else {
            return;
        };
        if self.history.session_id != id {
            self.history = HistoryState::new(&id);
            self.history_stale = true;
        }
        if self.status_sid != id {
            self.status_sid = id;
            self.status_offset = 0;
        }
    }

    /// The session whose newest History page the driver should read now, if one is due.
    pub fn history_request(&mut self) -> Option<String> {
        if self.tab != Tab::History || !self.history_stale || self.selected().is_none() {
            return None;
        }
        self.history_stale = false;
        Some(self.history.session_id.clone())
    }

    pub fn history_loaded(&mut self, session_id: &str, page: Vec<MessageRow>) {
        if self.history.session_id == session_id {
            self.history.apply_newest(page);
        }
    }

    pub fn older_loaded(&mut self, session_id: &str, page: Vec<MessageRow>) {
        let (w, h) = self.right_size();
        if self.history.session_id == session_id {
            self.history.apply_older(page, w, h);
        }
    }

    pub fn search_done(&mut self, query: String, hits: Vec<SearchHit>) {
        self.history.results(query, hits);
    }

    fn right_size(&self) -> (usize, usize) {
        let a = areas(self.area, self.warning.is_some());
        (
            usize::from(a.right.width).max(10),
            usize::from(a.right.height).max(1),
        )
    }

    /// A paste goes into the search box when it is open, and nowhere else.
    pub fn paste(&mut self, text: &str) {
        if self.tab == Tab::History && self.history.captures() {
            self.history.paste(text);
        }
    }

    fn pane_outcome(&self, o: Outcome) -> Action {
        match o {
            Outcome::None => Action::None,
            Outcome::Search(q) => Action::Search(q),
            Outcome::LoadOlder(ts, uuid) => Action::LoadOlder(ts, uuid),
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        self.sync_selection();
        let (w, h) = self.right_size();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Ctrl-C quits from anywhere, also from the search box (raw mode has no SIGINT).
        if ctrl && key.code == KeyCode::Char('c') {
            return Action::Quit;
        }
        if self.tab == Tab::History && self.selected().is_some() && self.history.captures() {
            let o = self.history.handle_key(key, w, h);
            return self.pane_outcome(o);
        }
        match key.code {
            // Ctrl or Alt chords are not commands.
            KeyCode::Char(_) if ctrl || key.modifiers.contains(KeyModifiers::ALT) => {
                return Action::None;
            }
            // There is no Send pane and so no draft: q quits straight away.
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Char('r') => return Action::Refresh,
            KeyCode::Char('1') => self.tab = Tab::Status,
            KeyCode::Char('2') => {
                self.tab = Tab::History;
                self.history_stale = true;
            }
            KeyCode::Char('3') => self.tab = Tab::Send,
            KeyCode::Char('/') => {
                self.tab = Tab::History;
                self.history_stale = true;
                if self.selected().is_some() {
                    self.history.open_search();
                }
            }
            KeyCode::Up | KeyCode::Down => self.select(key.code == KeyCode::Up),
            KeyCode::Char('j') => {
                if let Some((pid, ended)) = self.selected_pid() {
                    if ended {
                        self.show_toast("could not focus: session ended", Instant::now());
                    } else {
                        return Action::Focus(pid);
                    }
                }
            }
            _ => return self.pane_key(key, w, h),
        }
        Action::None
    }

    fn select(&mut self, up: bool) {
        let groups = self.groups();
        let flat: Vec<&str> = groups
            .iter()
            .flat_map(|g| g.sessions.iter().map(|s| s.session_id.as_str()))
            .collect();
        if flat.is_empty() {
            return;
        }
        let cur = self
            .selected()
            .and_then(|s| flat.iter().position(|id| *id == s.session_id))
            .unwrap_or(0);
        let next = if up {
            cur.saturating_sub(1)
        } else {
            (cur + 1).min(flat.len() - 1)
        };
        self.selected_id = Some(flat[next].to_string());
        self.sync_selection();
    }

    fn pane_key(&mut self, key: KeyEvent, w: usize, h: usize) -> Action {
        let Some(sel) = self.selected() else {
            return Action::None;
        };
        match self.tab {
            Tab::Status => {
                let total = status_lines(sel, &self.snapshot_meta(), self.now_ms, w);
                let max = max_offset(&total, h);
                let cur = self.status_offset.min(max);
                match key.code {
                    KeyCode::PageDown => self.status_offset = max.min(cur + h.saturating_sub(1)),
                    KeyCode::PageUp => self.status_offset = cur.saturating_sub(h.saturating_sub(1)),
                    _ => {}
                }
                Action::None
            }
            Tab::History => {
                let o = self.history.handle_key(key, w, h);
                self.pane_outcome(o)
            }
            Tab::Send => Action::None,
        }
    }

    fn snapshot_meta(&self) -> hub_core::view::snapshot::SnapshotMeta {
        self.snapshot
            .as_ref()
            .map(|s| s.meta.clone())
            .expect("a selected session implies a snapshot")
    }

    pub fn render(&self, f: &mut Frame) {
        let a = areas(f.area(), self.warning.is_some());
        let selected = self.selected();
        f.render_widget(Paragraph::new(self.header(selected)), a.header);

        let groups = self.groups();
        let empty = if self.snapshot.is_some() {
            "no sessions"
        } else {
            ""
        };
        let list = list_lines(
            &groups,
            selected.map(|s| s.session_id.as_str()),
            usize::from(a.list.width),
            usize::from(a.list.height),
            empty,
        );
        f.render_widget(Paragraph::new(list), a.list);
        let bar: Vec<TextLine> = (0..a.border.height).map(|_| TextLine::from("│")).collect();
        f.render_widget(Paragraph::new(bar), a.border);

        let w = usize::from(a.right.width).max(10);
        let h = usize::from(a.right.height).max(1);
        let lines = self.right_lines(selected, w, h);
        f.render_widget(
            Paragraph::new(lines.iter().map(Line::to_ratatui).collect::<Vec<_>>()),
            a.right,
        );

        f.render_widget(
            Paragraph::new(self.footer(usize::from(a.footer.width))),
            a.footer,
        );
        if let (Some(text), Some(r)) = (&self.warning, a.warning) {
            f.render_widget(
                Paragraph::new(TextLine::from(Span::styled(
                    truncate_end(&format!("! {text}"), usize::from(r.width)),
                    Style::default().fg(Color::Yellow),
                ))),
                r,
            );
        }
    }

    fn header(&self, selected: Option<&SessionView>) -> TextLine<'static> {
        let tab = |t: Tab, n: &str, label: &str| {
            let style = if self.tab == t {
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::default()
            };
            Span::styled(format!(" {n} {label} "), style)
        };
        TextLine::from(vec![
            Span::styled(
                "Session Hub ",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            tab(Tab::Status, "1", "Status"),
            tab(Tab::History, "2", "History"),
            tab(Tab::Send, "3", "Send"),
            Span::styled(
                selected.map_or(String::new(), |s| {
                    format!("  {}", session_label(&s.name, &s.session_id))
                }),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ])
    }

    fn right_lines(&self, selected: Option<&SessionView>, w: usize, h: usize) -> Vec<Line> {
        let dim = |t: &str| {
            vec![Line {
                text: t.into(),
                dim: true,
                ..Line::default()
            }]
        };
        let Some(snap) = &self.snapshot else {
            return dim(if self.store_error.is_some() {
                "store unavailable"
            } else {
                "loading..."
            });
        };
        let Some(sel) = selected else {
            return dim("no sessions yet");
        };
        match self.tab {
            Tab::Status => {
                let lines = status_lines(sel, &snap.meta, self.now_ms, w);
                visible(&lines, self.status_offset, h)
            }
            Tab::History => self.history.lines(&snap.sessions, w, h),
            Tab::Send => dim(SEND_LINE),
        }
    }

    fn footer(&self, width: usize) -> TextLine<'static> {
        let (text, style) = if let Some(e) = self.error() {
            (format!("store error: {e}"), Style::default().fg(Color::Red))
        } else if let Some((t, _)) = &self.toast {
            (t.clone(), Style::default().fg(Color::Yellow))
        } else {
            (
                "↑↓ select · 1/2/3 pane · / search · j jump · r refresh · q quit · times local"
                    .to_string(),
                Style::default().add_modifier(Modifier::DIM),
            )
        };
        TextLine::from(Span::styled(truncate_end(&text, width), style))
    }
}
