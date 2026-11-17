//! The History pane (`src/tui/HistoryPane.tsx`): the session's messages in pages of 50 with
//! PgUp / PgDn / End, and the `/` search over every session.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hub_core::security::{clean_input, clean_text};
use hub_core::store::{MessageRow, Role, SearchHit, SessionView};
use hub_core::text::clip_utf16;
use ratatui::style::Color;

use crate::format::{hhmm, role_label, session_label, wrap};
use crate::status_pane::Line;

pub const PAGE: usize = 50;
pub const SEARCH_LIMIT: i64 = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Search {
    Off,
    Input(String),
    Results { query: String, hits: Vec<SearchHit> },
}

/// Scroll position: the top visible line is `offset` lines into message `uuid`. None follows the
/// newest message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub uuid: String,
    pub offset: usize,
}

/// What a key asks the caller to do with the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    None,
    Search(String),
    /// The page before the oldest loaded message: `(ts, uuid)`.
    LoadOlder(String, String),
}

fn clean_message(m: MessageRow) -> MessageRow {
    MessageRow {
        text: clean_text(&m.text),
        from_name: m.from_name.map(|n| clean_text(&n)),
        ..m
    }
}

fn role_color(role: Role) -> Color {
    match role {
        Role::Owner => Color::Green,
        Role::Peer => Color::Magenta,
        Role::Assistant => Color::Cyan,
    }
}

struct Layout {
    lines: Vec<Line>,
    /// Index into the messages of the message each line belongs to.
    owner: Vec<usize>,
    /// First line of each message.
    start: Vec<usize>,
}

fn layout(messages: &[MessageRow], width: usize) -> Layout {
    let mut lay = Layout {
        lines: Vec::new(),
        owner: Vec::new(),
        start: Vec::new(),
    };
    for (i, m) in messages.iter().enumerate() {
        lay.start.push(lay.lines.len());
        let mut add = |l: Line| {
            lay.lines.push(l);
            lay.owner.push(i);
        };
        add(Line {
            text: format!(
                "{} {}",
                hhmm(&m.ts),
                role_label(m.role, m.from_name.as_deref())
            ),
            color: Some(role_color(m.role)),
            bold: true,
            dim: false,
        });
        for t in wrap(&m.text, width, "") {
            add(Line::plain(t));
        }
    }
    lay
}

fn bottom(lay: &Layout, h: usize) -> usize {
    lay.lines.len().saturating_sub(h)
}

/// Index of the top visible line for an anchor; None or an unknown uuid means the bottom.
fn top_of(lay: &Layout, messages: &[MessageRow], anchor: Option<&Anchor>, h: usize) -> usize {
    let Some(a) = anchor else {
        return bottom(lay, h);
    };
    match messages.iter().position(|m| m.uuid == a.uuid) {
        Some(i) => bottom(lay, h).min(lay.start[i] + a.offset),
        None => bottom(lay, h),
    }
}

fn anchor_at(lay: &Layout, messages: &[MessageRow], top: usize, h: usize) -> Option<Anchor> {
    if top >= bottom(lay, h) {
        return None;
    }
    let i = *lay.owner.get(top)?;
    Some(Anchor {
        uuid: messages[i].uuid.clone(),
        offset: top - lay.start[i],
    })
}

/// State of one session's History pane. Replaced whenever the selected session changes.
#[derive(Debug, Clone)]
pub struct HistoryState {
    pub session_id: String,
    pub messages: Vec<MessageRow>,
    pub exhausted: bool,
    pub anchor: Option<Anchor>,
    pub search: Search,
}

impl HistoryState {
    pub fn new(session_id: &str) -> HistoryState {
        HistoryState {
            session_id: session_id.to_string(),
            messages: Vec::new(),
            exhausted: false,
            anchor: None,
            search: Search::Off,
        }
    }

    /// True while the pane takes every key (the search input).
    pub fn captures(&self) -> bool {
        matches!(self.search, Search::Input(_))
    }

    pub fn open_search(&mut self) {
        self.search = Search::Input(String::new());
    }

    pub fn paste(&mut self, text: &str) {
        if let Search::Input(q) = &mut self.search {
            q.push_str(&clean_input(text, false).replace('\n', " "));
        }
    }

    /// Keeps older pages already loaded and replaces the newest page with a fresh read.
    pub fn apply_newest(&mut self, page: Vec<MessageRow>) {
        let page: Vec<MessageRow> = page.into_iter().map(clean_message).collect();
        let Some(first) = page.first() else {
            return;
        };
        let first_ts = first.ts.clone();
        let mut next: Vec<MessageRow> = std::mem::take(&mut self.messages)
            .into_iter()
            .filter(|m| m.ts <= first_ts && !page.iter().any(|p| p.uuid == m.uuid))
            .collect();
        next.extend(page);
        self.messages = next;
    }

    fn more_older(&self) -> bool {
        !self.exhausted && self.messages.len() >= PAGE
    }

    fn page_up(&mut self, width: usize, h: usize) -> Outcome {
        let lay = layout(&self.messages, width);
        let top = top_of(&lay, &self.messages, self.anchor.as_ref(), h) as i64;
        let target = top - (h as i64 - 1);
        if target < 0
            && self.more_older()
            && let Some(oldest) = self.messages.first()
        {
            return Outcome::LoadOlder(oldest.ts.clone(), oldest.uuid.clone());
        }
        self.anchor = anchor_at(&lay, &self.messages, target.max(0) as usize, h);
        Outcome::None
    }

    /// The older page the store returned for [`Outcome::LoadOlder`]: scrolls on from there.
    pub fn apply_older(&mut self, page: Vec<MessageRow>, width: usize, h: usize) {
        let Some(oldest) = self.messages.first().map(|m| m.uuid.clone()) else {
            return;
        };
        let lay = layout(&self.messages, width);
        let mut top = top_of(&lay, &self.messages, self.anchor.as_ref(), h) as i64;
        if page.len() < PAGE {
            self.exhausted = true;
        }
        let mut next: Vec<MessageRow> = page
            .into_iter()
            .map(clean_message)
            .filter(|m| !self.messages.iter().any(|o| o.uuid == m.uuid))
            .collect();
        next.append(&mut self.messages);
        self.messages = next;
        let grown = layout(&self.messages, width);
        let at = self
            .messages
            .iter()
            .position(|m| m.uuid == oldest)
            .map_or(0, |i| grown.start[i]);
        top += at as i64;
        let target = top - (h as i64 - 1);
        self.anchor = anchor_at(&grown, &self.messages, target.max(0) as usize, h);
    }

    fn page_down(&mut self, width: usize, h: usize) {
        let lay = layout(&self.messages, width);
        let top = top_of(&lay, &self.messages, self.anchor.as_ref(), h);
        self.anchor = anchor_at(&lay, &self.messages, top + (h - 1), h);
    }

    pub fn results(&mut self, query: String, hits: Vec<SearchHit>) {
        self.search = Search::Results { query, hits };
    }

    pub fn handle_key(&mut self, key: KeyEvent, width: usize, h: usize) -> Outcome {
        let h = h.max(1);
        match &mut self.search {
            Search::Input(q) => {
                match key.code {
                    KeyCode::Esc => self.search = Search::Off,
                    KeyCode::Enter => return Outcome::Search(q.clone()),
                    KeyCode::Backspace | KeyCode::Delete => {
                        q.pop();
                    }
                    KeyCode::Char(c)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        q.push_str(&clean_input(&c.to_string(), false).replace('\n', " "));
                    }
                    _ => {}
                }
                Outcome::None
            }
            Search::Results { .. } => {
                if key.code == KeyCode::Esc {
                    self.search = Search::Off;
                }
                Outcome::None
            }
            Search::Off => match key.code {
                KeyCode::PageUp => self.page_up(width, h),
                KeyCode::PageDown => {
                    self.page_down(width, h);
                    Outcome::None
                }
                KeyCode::End => {
                    self.anchor = None;
                    Outcome::None
                }
                _ => Outcome::None,
            },
        }
    }

    /// The `h` lines the pane shows at `width` columns. `sessions` names the hits of a search.
    pub fn lines(&self, sessions: &[SessionView], width: usize, h: usize) -> Vec<Line> {
        let h = h.max(1);
        match &self.search {
            Search::Input(q) => {
                return vec![
                    Line {
                        text: format!("Search: {q}_"),
                        bold: true,
                        ..Line::default()
                    },
                    Line {
                        text: "Enter runs the search; words match as whole terms. Esc cancels."
                            .into(),
                        dim: true,
                        ..Line::default()
                    },
                ];
            }
            Search::Results { query, hits } => {
                let mut v = hit_lines(query, hits, sessions, width);
                v.truncate(h);
                return v;
            }
            Search::Off => {}
        }
        if self.messages.is_empty() {
            return vec![Line {
                text: "no messages yet".into(),
                dim: true,
                ..Line::default()
            }];
        }
        let lay = layout(&self.messages, width);
        let top = top_of(&lay, &self.messages, self.anchor.as_ref(), h);
        let hint = top == 0 && !self.exhausted && self.messages.len() >= PAGE;
        let rows = if hint { h - 1 } else { h };
        let mut shown: Vec<Line> = Vec::new();
        if hint {
            shown.push(Line {
                text: "... PgUp loads older messages".into(),
                dim: true,
                ..Line::default()
            });
        }
        shown.extend(lay.lines.iter().skip(top).take(rows).cloned());
        shown
    }
}

fn hit_lines(query: &str, hits: &[SearchHit], sessions: &[SessionView], w: usize) -> Vec<Line> {
    let n = hits.len();
    let mut out = vec![Line {
        text: format!(
            "Search \"{query}\": {n} result{} (Esc clears)",
            if n == 1 { "" } else { "s" }
        ),
        bold: true,
        ..Line::default()
    }];
    for hit in hits {
        let who = sessions
            .iter()
            .find(|s| s.session_id == hit.session_id)
            .map_or_else(
                || clip_utf16(&clean_text(&hit.session_id), 8).to_string(),
                |s| session_label(&s.name, &s.session_id),
            );
        let role = role_label(hit.role, None);
        let text = format!(
            "{} {who} {role}: {}",
            hhmm(&hit.ts),
            clean_text(&hit.snippet)
        );
        for t in wrap(&text, w, "  ") {
            out.push(Line::plain(t));
        }
    }
    out
}
