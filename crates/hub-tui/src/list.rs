//! The session list (`src/tui/SessionList.tsx`): grouped by profile, name row and cwd row each.
use hub_core::store::SessionView;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span};

use crate::format::{profile_header, session_label, state_label};

const PROFILE_ORDER: [&str; 2] = ["claude", "claude-personal"];

pub struct Group<'a> {
    pub profile: String,
    pub sessions: Vec<&'a SessionView>,
}

/// Known profiles first in a fixed order, others alphabetically; store order kept inside a group.
pub fn group_sessions(sessions: &[SessionView]) -> Vec<Group<'_>> {
    let mut groups: Vec<Group<'_>> = Vec::new();
    for s in sessions {
        match groups.iter_mut().find(|g| g.profile == s.profile) {
            Some(g) => g.sessions.push(s),
            None => groups.push(Group {
                profile: s.profile.clone(),
                sessions: vec![s],
            }),
        }
    }
    let rank = |p: &str| {
        PROFILE_ORDER
            .iter()
            .position(|k| *k == p)
            .unwrap_or(PROFILE_ORDER.len())
    };
    groups.sort_by(|a, b| {
        rank(&a.profile)
            .cmp(&rank(&b.profile))
            .then_with(|| a.profile.cmp(&b.profile))
    });
    groups
}

/// Terminal columns of `s`.
pub fn cell_width(s: &str) -> usize {
    Span::raw(s).width()
}

fn char_width(c: char) -> usize {
    Span::raw(c.to_string()).width()
}

/// Keeps the start of `s` within `w` columns, ending in an ellipsis when cut.
pub fn truncate_end(s: &str, w: usize) -> String {
    if cell_width(s) <= w {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = char_width(c);
        if used + cw + 1 > w {
            break;
        }
        out.push(c);
        used += cw;
    }
    if w > 0 {
        out.push('…');
    }
    out
}

/// Keeps the end of `s` within `w` columns, starting with an ellipsis when cut.
pub fn truncate_start(s: &str, w: usize) -> String {
    if cell_width(s) <= w {
        return s.to_string();
    }
    let mut tail: Vec<char> = Vec::new();
    let mut used = 0;
    for c in s.chars().rev() {
        let cw = char_width(c);
        if used + cw + 1 > w {
            break;
        }
        tail.push(c);
        used += cw;
    }
    tail.reverse();
    let mut out = String::new();
    if w > 0 {
        out.push('…');
    }
    out.extend(tail);
    out
}

fn state_color(label: &str) -> Option<Color> {
    match label {
        "needs you" => Some(Color::Yellow),
        "busy" => Some(Color::Cyan),
        "ended" => Some(Color::Gray),
        _ => None,
    }
}

/// The list rows for `height` lines of `width` columns, scrolled to keep the selection centred.
/// `empty_text` is shown when there are no groups ("" while the first read is pending).
pub fn list_lines(
    groups: &[Group<'_>],
    selected_id: Option<&str>,
    width: usize,
    height: usize,
    empty_text: &str,
) -> Vec<TextLine<'static>> {
    let mut rows: Vec<TextLine<'static>> = Vec::new();
    let mut selected_row = 0;
    for g in groups {
        rows.push(TextLine::from(Span::styled(
            truncate_end(&profile_header(&g.profile), width),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for s in &g.sessions {
            let selected = selected_id == Some(s.session_id.as_str());
            if selected {
                selected_row = rows.len();
            }
            let label = state_label(s.state, s.ended_at);
            let badge = if s.waiting_count > 0 {
                format!(" [{}]", s.waiting_count)
            } else {
                String::new()
            };
            let right = format!("{badge} {label}");
            let right_w = cell_width(&right);
            let left_w = width.saturating_sub(right_w);
            let left = format!(
                "{}{}",
                if selected { "> " } else { "  " },
                session_label(&s.name, &s.session_id)
            );
            let left = truncate_end(&left, left_w);
            // The right part sits at the right edge; the gap is part of the name's own box.
            let pad = left_w.saturating_sub(cell_width(&left));
            let name_style = if selected {
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::default()
            };
            let mut state_style = Style::default();
            if let Some(c) = state_color(&label) {
                state_style = state_style.fg(c);
            }
            rows.push(TextLine::from(vec![
                Span::styled(left, name_style),
                Span::raw(" ".repeat(pad)),
                Span::styled(badge, Style::default().fg(Color::Yellow)),
                Span::styled(format!(" {label}"), state_style),
            ]));
            rows.push(TextLine::from(Span::styled(
                format!("  {}", truncate_start(&s.cwd, width.saturating_sub(2))),
                Style::default().add_modifier(Modifier::DIM),
            )));
        }
    }
    if rows.is_empty() {
        if !empty_text.is_empty() {
            rows.push(TextLine::from(Span::styled(
                empty_text.to_string(),
                Style::default().add_modifier(Modifier::DIM),
            )));
        }
        return rows;
    }
    let h = height.max(1);
    let max_start = rows.len().saturating_sub(h);
    let start = max_start.min(selected_row.saturating_sub(h / 2));
    rows.into_iter().skip(start).take(h).collect()
}
