//! The Status pane (`src/tui/StatusPane.tsx`): Progress, What's remaining, Waiting on you.
use hub_core::security::clean_text;
use hub_core::store::SessionView;
use hub_core::view::snapshot::SnapshotMeta;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span};

use crate::format::{ago, hhmm, parse_ts, profile_header, wrap};

/// One display line with its style (`Line` in StatusPane.tsx).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    pub color: Option<Color>,
    pub bold: bool,
    pub dim: bool,
}

impl Line {
    pub fn plain(text: impl Into<String>) -> Line {
        Line {
            text: text.into(),
            ..Line::default()
        }
    }

    /// Renders one line; `clean_text` here is a last guard, store text is already cleaned on read.
    pub fn to_ratatui(&self) -> TextLine<'static> {
        let mut style = Style::default();
        if let Some(c) = self.color {
            style = style.fg(c);
        }
        if self.bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.dim {
            style = style.add_modifier(Modifier::DIM);
        }
        let text = if self.text.is_empty() {
            " ".to_string()
        } else {
            clean_text(&self.text)
        };
        TextLine::from(Span::styled(text, style))
    }
}

struct Out {
    lines: Vec<Line>,
    width: usize,
}

impl Out {
    fn add(&mut self, text: &str, style: Line, indent: &str) {
        for t in wrap(text, self.width, indent) {
            self.lines.push(Line {
                text: t,
                ..style.clone()
            });
        }
    }

    fn blank(&mut self) {
        self.lines.push(Line::plain(""));
    }
}

fn red() -> Line {
    Line {
        color: Some(Color::Red),
        ..Line::default()
    }
}
fn dim() -> Line {
    Line {
        dim: true,
        ..Line::default()
    }
}
fn bold() -> Line {
    Line {
        bold: true,
        ..Line::default()
    }
}

/// Pure: the Status pane's lines for one session, wrapped to `width`.
pub fn status_lines(s: &SessionView, meta: &SnapshotMeta, now: i64, width: usize) -> Vec<Line> {
    let mut o = Out {
        lines: Vec::new(),
        width,
    };
    for st in &meta.stale {
        o.add(
            &format!(
                "stale: {} list not updated for {}",
                profile_header(&st.profile),
                ago(now - st.since)
            ),
            red(),
            "",
        );
    }
    if meta.unknown_count > 0 {
        let v = match meta.unknown_version.as_deref() {
            Some(v) if !v.is_empty() => format!(" (version {v})"),
            _ => String::new(),
        };
        o.add(
            &format!(
                "format changed: {} unknown records skipped{v}",
                meta.unknown_count
            ),
            red(),
            "",
        );
    }
    if meta.relay_stray_count > 0 {
        let n = meta.relay_stray_count;
        o.add(
            &format!(
                "relay: {n} message{} reached the wrong session (see run\\relay.log)",
                if n == 1 { "" } else { "s" }
            ),
            red(),
            "",
        );
    }
    if meta.unreadable_sessions > 0 {
        let n = meta.unreadable_sessions;
        o.add(
            &format!(
                "index: {n} session row{} unreadable, not listed",
                if n == 1 { "" } else { "s" }
            ),
            red(),
            "",
        );
    }
    let Some(st) = &s.status else {
        o.add("no status reply yet", dim(), "");
        return o.lines;
    };
    if let Some(lr) = &s.latest_reply
        && !lr.has_status
        && lr.reply_uuid != st.reply_uuid
        && lr.reply_ts > st.reply_ts
    {
        o.add(
            &format!("newer reply at {} has no status block", hhmm(&lr.reply_ts)),
            Line {
                color: Some(Color::Yellow),
                ..Line::default()
            },
            "",
        );
    }
    let age = match parse_ts(&st.reply_ts) {
        Some(a) => format!(
            "status from {} ago (reply at {} local)",
            ago(now - a),
            hhmm(&st.reply_ts)
        ),
        None => "status age unknown".to_string(),
    };
    o.add(&age, dim(), "");
    if st.progress_pct.is_some() || st.phase.is_some() {
        let pct = match st.progress_pct {
            Some(p) => format!("{p}% overall"),
            None => "progress ?".to_string(),
        };
        o.add(
            &format!(
                "Progress: {pct}. Phase: {}",
                st.phase.as_deref().unwrap_or("?")
            ),
            bold(),
            "",
        );
    }
    o.blank();
    o.add("What's remaining", bold(), "");
    if st.remaining.is_empty() {
        o.add("  nothing", dim(), "");
    }
    for (i, r) in st.remaining.iter().enumerate() {
        o.add(&format!("  {}. {r}", i + 1), Line::default(), "     ");
    }
    o.blank();
    o.add("Waiting on you", bold(), "");
    // An item the owner already answered after this list was posted is left out.
    let open: Vec<_> = st.waiting.iter().filter(|w| !w.answered).collect();
    if open.is_empty() {
        o.add("  nothing", dim(), "");
    }
    for w in open {
        let id = w.item.id.as_deref().filter(|i| !i.is_empty());
        let id = id.map_or(String::new(), |i| format!("{i} "));
        let new = if w.item.is_new { "NEW " } else { "" };
        let style = if w.item.is_new {
            Line {
                color: Some(Color::Yellow),
                ..Line::default()
            }
        } else {
            Line::default()
        };
        o.add(&format!("  {id}{new}{}", w.item.text), style, "    ");
        for c in &w.item.children {
            o.add(&format!("    - {c}"), dim(), "      ");
        }
    }
    if !st.recommendations.is_empty() {
        o.blank();
        o.add("Recommendations", bold(), "");
        for r in &st.recommendations {
            o.add(&format!("  * {r}"), Line::default(), "    ");
        }
    }
    o.lines
}

/// The window of `lines` the pane shows at `offset`, with the "more lines" marker when cut.
pub fn visible(lines: &[Line], offset: usize, h: usize) -> Vec<Line> {
    let h = h.max(1);
    let max_offset = lines.len().saturating_sub(h);
    let off = offset.min(max_offset);
    let more = lines.len() as i64 - off as i64 - h as i64;
    if more > 0 {
        let mut v: Vec<Line> = lines[off..off + h - 1].to_vec();
        v.push(Line {
            text: format!("... {} more lines (PgDn)", more + 1),
            dim: true,
            ..Line::default()
        });
        v
    } else {
        lines[off..(off + h).min(lines.len())].to_vec()
    }
}

/// The largest scroll offset for `lines` in a pane `h` rows high.
pub fn max_offset(lines: &[Line], h: usize) -> usize {
    lines.len().saturating_sub(h.max(1))
}
