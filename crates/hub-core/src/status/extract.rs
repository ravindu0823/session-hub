//! "Progress / What's remaining / Waiting on you" extraction, a port of `src/status/extract.ts`
//! that gives the same output for every input.
//!
//! JS translation (see [`js_regex`]): `\s` / `\S` are JS whitespace ([`JS_SPACE_CLASS`]), `.` is
//! "not a JS line terminator", `\d` is `[0-9]`, the `i` flag (without `u`) folds ASCII only, so
//! case-insensitive words are `(?i-u:...)`. Look-around is gone:
//! - a one-character look-ahead that ends a pattern (`ID_RE`) is consumed, which picks the same
//!   capture since nothing follows it;
//! - `(?=\S)` before `[^*\n]*?\S` in the bold rule becomes the alternation `\S|[^*\n\s][^*\n]*?\S`
//!   (same candidates in the same order);
//! - the underscore rule (a back-reference and `(?!\w)`) and the `(?=\S)` of the dash separator are
//!   scanned by hand.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::{Captures, Regex};
use serde::{Deserialize, Serialize};

use crate::text::{JS_SPACE_CLASS, is_js_space, split_js_lines, utf16_len};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitingItem {
    pub id: Option<String>,
    pub text: String,
    pub is_new: bool,
    pub children: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub progress_pct: Option<i64>,
    pub phase: Option<String>,
    pub remaining: Vec<String>,
    pub waiting: Vec<WaitingItem>,
    pub recommendations: Vec<String>,
}

/// Compiles a pattern written in JS terms: `\s` becomes `[JS_SPACE_CLASS]` (also inside a class,
/// where it nests as a union), `\S` its complement, and `\N` stands for JS `.` without the `s` flag.
pub(super) fn js_regex(pattern: &str) -> Regex {
    let translated = pattern
        .replace(r"\S", &format!("[^{JS_SPACE_CLASS}]"))
        .replace(r"\s", &format!("[{JS_SPACE_CLASS}]"))
        .replace(r"\N", r"[^\n\r\x{2028}\x{2029}]");
    Regex::new(&translated).unwrap_or_else(|e| panic!("status pattern {pattern:?}: {e}"))
}

macro_rules! re {
    ($name:ident, $pattern:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| js_regex($pattern));
    };
}

re!(
    PROGRESS_RE,
    r"^\s*\*{0,2}(?i-u:progress):\s*(?:(?i-u:about)\s+)?([0-9]{1,3}(?:\.[0-9]+)?)%\s+(?i-u:overall)\.\s+(?i-u:phase):\s*(\N+?)\.?(?:\*\*|\s*$)"
);
re!(
    REMAINING_HASH,
    r"^\s{0,3}#{2,4}\s*\**(?i-u:what)['\x{2019}](?i-u:s remaining):?\**:?\s*#*\s*$"
);
re!(
    WAITING_HASH,
    r"^\s{0,3}#{2,4}\s*\**(?i-u:waiting on you):?\**:?\s*#*\s*$"
);
re!(
    REMAINING_BOLD,
    r"^\s*\*\*(?i-u:what)['\x{2019}](?i-u:s remaining):?\*\*:?\s*(\N*)$"
);
re!(
    WAITING_BOLD,
    r"^\s*\*\*(?i-u:waiting on you):?\*\*:?\s*(\N*)$"
);
re!(ANY_HEADING, r"^\s{0,3}#{1,6}(?:\s|$)");
re!(THEMATIC_BREAK, r"^\s*(?:-{3,}|\*{3,}|_{3,})\s*$");
re!(TABLE_ROW, r"^\s*\|");
re!(BULLET_RE, r"^(\s*)(?:[0-9]+[.)]|[\-*\x{2022}])\s+(\N*)$");
re!(NEW_RE, r"^(\*{0,2})(?i-u:new):(\*{0,2})\s*");
// JS: `(...)(?=[:\s,–—-]|$)`; the look-ahead character is consumed, group 1 is the ID.
re!(
    ID_RE,
    r"^((?:[A-Z][A-Z0-9]{0,7}-)*[A-Z][A-Z0-9]{0,6}[0-9])(?:[:\s,\x{2013}\x{2014}\-]|$)"
);
re!(
    RANGE_RE,
    r"^[\x{2013}\x{2014}\-]\s*[A-Z][A-Z0-9]{0,7}(?:-[A-Z][A-Z0-9]{0,7})*[0-9]"
);
re!(RECOMMEND_RE, r"(?i-u:recommend|my pick|\(recommended\))");
re!(
    RECOMMEND_LEAD,
    r"^\s*(?:[0-9]+[.)]|[\-*\x{2022}]|#{1,6})\s+"
);
re!(NOTHING_RE, r"^(?i-u:nothing)\.?$");
re!(FENCE_RE, r"^\s*```");
re!(CODE_START_RE, r"^[*_\s]*`");
re!(CODE_SPAN, r"`([^`\n]*)`");
// JS: `\*{1,3}(?=\S)([^*\n]*?\S)\*{1,3}`.
re!(BOLD_RE, r"\*{1,3}(\S|[^*\n\s][^*\n]*?\S)\*{1,3}");
re!(SPACES_RE, r"\s+");
re!(HOLDER_RE, r"\x{E000}([0-9]+)\x{E000}");

const MAX_ID_CHARS: usize = 24;
const MAX_RECOMMENDATIONS: usize = 5;
const CODE_MARK: char = '\u{E000}';

fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

fn trim_start(s: &str) -> &str {
    s.trim_start_matches(is_js_space)
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// JS `Math.round`: the nearest integer, halves toward +infinity (exact for every finite `x`).
fn js_round(x: f64) -> f64 {
    let floor = x.floor();
    if x - floor >= 0.5 { floor + 1.0 } else { floor }
}

/// JS `Number(digits)`: `NaN` for an empty string.
fn number(digits: &str) -> f64 {
    digits.parse().unwrap_or(f64::NAN)
}

/// JS `(^|\W)(_{1,2})(?=\S)([^_\n]*?\S)\2(?!\w)` tried at `p`, in backtracking order: the end of
/// group 1, group 3 and the end of the match.
fn underscore_at(s: &str, p: usize) -> Option<(usize, &str, usize)> {
    let mut leads = Vec::with_capacity(2);
    if p == 0 {
        leads.push(0);
    }
    if let Some(c) = s[p..].chars().next().filter(|&c| !is_word(c)) {
        leads.push(p + c.len_utf8());
    }
    for lead in leads {
        for n in [2, 1] {
            let q = lead + n;
            if s.len() < q || s.as_bytes()[lead..q].iter().any(|&b| b != b'_') {
                continue;
            }
            if let Some(e) = underscore_body_end(s, q, n) {
                return Some((lead, &s[q..e], e + n));
            }
        }
    }
    None
}

/// End of `(?=\S)([^_\n]*?\S)` from `q` such that `n` underscores and then a non-word follow.
fn underscore_body_end(s: &str, q: usize, n: usize) -> Option<usize> {
    let rest = &s[q..];
    if rest.chars().next().is_none_or(is_js_space) {
        return None;
    }
    for (k, c) in rest.char_indices() {
        if !is_js_space(c) {
            let e = q + k + c.len_utf8();
            let close = &s[e..];
            if close.len() >= n
                && close.as_bytes()[..n].iter().all(|&b| b == b'_')
                && !close[n..].chars().next().is_some_and(is_word)
            {
                return Some(e);
            }
        }
        if c == '_' || c == '\n' {
            break;
        }
    }
    None
}

/// JS `s.replace(/(^|\W)(_{1,2})(?=\S)([^_\n]*?\S)\2(?!\w)/g, "$1$3")`.
fn strip_underscores(s: &str) -> Cow<'_, str> {
    let mut out = String::new();
    let mut last = 0;
    let mut pos = 0;
    while pos < s.len() {
        match underscore_at(s, pos) {
            Some((lead_end, body, end)) => {
                out.push_str(&s[last..lead_end]);
                out.push_str(body);
                last = end;
                pos = end;
            }
            None => pos += s[pos..].chars().next().map_or(1, char::len_utf8),
        }
    }
    if last == 0 {
        return Cow::Borrowed(s);
    }
    out.push_str(&s[last..]);
    Cow::Owned(out)
}

fn strip_markdown(s: &str) -> String {
    let mut codes: Vec<String> = Vec::new();
    let masked = CODE_SPAN.replace_all(s, |c: &Captures| {
        codes.push(c[1].to_string());
        format!("{CODE_MARK}{}{CODE_MARK}", codes.len() - 1)
    });
    let bold = BOLD_RE.replace_all(&masked, |c: &Captures| c[1].to_string());
    let plain = strip_underscores(&bold);
    let spaced = SPACES_RE.replace_all(&plain, " ");
    HOLDER_RE
        .replace_all(js_trim(&spaced), |c: &Captures| {
            let i = number(&c[1]);
            // `as` saturates, so an index past the list (or a huge number) gives "".
            codes.get(i as usize).cloned().unwrap_or_default()
        })
        .into_owned()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Remaining,
    Waiting,
}

fn match_heading(line: &str) -> Option<(Kind, String)> {
    if REMAINING_HASH.is_match(line) {
        return Some((Kind::Remaining, String::new()));
    }
    if WAITING_HASH.is_match(line) {
        return Some((Kind::Waiting, String::new()));
    }
    if let Some(r) = REMAINING_BOLD.captures(line) {
        return Some((Kind::Remaining, js_trim(&r[1]).to_string()));
    }
    WAITING_BOLD
        .captures(line)
        .map(|w| (Kind::Waiting, js_trim(&w[1]).to_string()))
}

/// Lines outside ``` fences; JS `markdown.split(/\r?\n/)`.
fn unfenced(markdown: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for line in split_js_lines(markdown) {
        if FENCE_RE.is_match(line) {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence {
            out.push(line);
        }
    }
    out
}

struct RawItem {
    text: String,
    children: Vec<String>,
}

/// JS `ws.replace(/\t/g, "    ").length` (whitespace is all in the BMP).
fn indent_of(ws: &str) -> usize {
    ws.chars().map(|c| if c == '\t' { 4 } else { 1 }).sum()
}

fn parse_items(lines: &[Cow<'_, str>]) -> Vec<RawItem> {
    let mut items: Vec<RawItem> = Vec::new();
    let has_bullets = lines.iter().any(|l| BULLET_RE.is_match(l));
    let mut top_indent: Option<usize> = None;
    let mut last_is_child = false;
    let mut last_indent = 0;
    let mut blank_seen = false;
    for line in lines {
        let line: &str = line;
        if js_trim(line).is_empty() {
            blank_seen = true;
            continue;
        }
        if let Some(m) = BULLET_RE.captures(line) {
            let indent = indent_of(&m[1]);
            let body = js_trim(&m[2]).to_string();
            let top = *top_indent.get_or_insert(indent);
            match items.last_mut() {
                Some(last) if indent > top => {
                    last.children.push(body);
                    last_is_child = true;
                }
                _ => {
                    items.push(RawItem {
                        text: body,
                        children: Vec::new(),
                    });
                    last_is_child = false;
                }
            }
            last_indent = indent;
            blank_seen = false;
            continue;
        }
        let indent = indent_of(&line[..line.len() - trim_start(line).len()]);
        let Some(last) = items.last_mut() else {
            if has_bullets {
                continue;
            }
            items.push(RawItem {
                text: js_trim(line).to_string(),
                children: Vec::new(),
            });
            last_is_child = false;
            last_indent = indent;
            blank_seen = false;
            continue;
        };
        if blank_seen && indent <= last_indent {
            break;
        }
        blank_seen = false;
        let target = match last.children.last_mut() {
            Some(child) if last_is_child => child,
            _ => &mut last.text,
        };
        target.push(' ');
        target.push_str(js_trim(line));
    }
    if let [only] = items.as_slice()
        && only.children.is_empty()
        && NOTHING_RE.is_match(&strip_markdown(&only.text))
    {
        return Vec::new();
    }
    items
}

/// JS `id.replace(/\d+$/, "")`.
fn stem(id: &str) -> &str {
    id.trim_end_matches(|c: char| c.is_ascii_digit())
}

/// JS `Number(/\d+$/.exec(id)?.[0] ?? NaN)`.
fn num(id: &str) -> f64 {
    number(&id[stem(id).len()..])
}

/// Same stem, or the second's stem is the first's last segments ("AUTH-Q1-Q3").
fn same_stem(first: &str, second: &str) -> bool {
    let (a, b) = (stem(first), stem(second));
    a == b || a.strip_suffix(b).is_some_and(|head| head.ends_with('-'))
}

/// The ID at the start of the text, or `None`; of a range ("Q1-Q3", "AUTH-Q1-Q3") the first ID.
fn id_at(text: &str) -> Option<&str> {
    let id = ID_RE.captures(text)?.get(1)?.as_str();
    if utf16_len(id) > MAX_ID_CHARS {
        return None;
    }
    for (i, _) in id.match_indices('-') {
        let (first, second) = (&id[..i], &id[i + 1..]);
        if first.ends_with(|c: char| c.is_ascii_digit())
            && same_stem(first, second)
            && num(second) > num(first)
        {
            return Some(first);
        }
    }
    Some(id)
}

/// JS `rest.replace(/^\s*:\s*/, "").replace(/^\s*[,–—-]\s+(?=\S)/, "").replace(/^\s+/, "")`.
fn item_body(rest: &str) -> &str {
    let after_colon = match trim_start(rest).strip_prefix(':') {
        Some(r) => trim_start(r),
        None => rest,
    };
    let mut chars = trim_start(after_colon).chars();
    let after_dash = match chars.next() {
        Some(',' | '\u{2013}' | '\u{2014}' | '-') => {
            let after = chars.as_str();
            let r = trim_start(after);
            if r.len() < after.len() && !r.is_empty() {
                r
            } else {
                after_colon
            }
        }
        _ => after_colon,
    };
    trim_start(after_dash)
}

fn to_waiting(raw: &RawItem) -> WaitingItem {
    let mut text: &str = &raw.text;
    let owned;
    let nm = NEW_RE.captures(text);
    let is_new = nm.is_some();
    if let Some(nm) = nm {
        // "**New: SH9** foo": the opening ** has no closing one in the prefix, keep it for the body.
        let reopen = if !nm[1].is_empty() && nm[2].is_empty() {
            &nm[1]
        } else {
            ""
        };
        owned = format!("{reopen}{}", &text[nm[0].len()..]);
        text = &owned;
    }
    let children: Vec<String> = raw.children.iter().map(|c| strip_markdown(c)).collect();
    let full = strip_markdown(text);
    let unnamed = |full: String| WaitingItem {
        id: None,
        text: full,
        is_new,
        children: children.clone(),
    };
    // An item that opens with a code span is quoted code, never an ID.
    if CODE_START_RE.is_match(text) {
        return unnamed(full);
    }
    let Some(id) = id_at(&full) else {
        return unnamed(full);
    };
    let rest = &full[id.len()..];
    let body = if RANGE_RE.is_match(rest) {
        ""
    } else {
        item_body(rest)
    };
    WaitingItem {
        id: Some(id.to_string()),
        text: if body.is_empty() {
            full.clone()
        } else {
            body.to_string()
        },
        is_new,
        children,
    }
}

fn to_remaining(raw: &RawItem) -> String {
    let head = strip_markdown(&raw.text);
    let kids: Vec<String> = raw
        .children
        .iter()
        .map(|c| strip_markdown(c))
        .filter(|c| !c.is_empty())
        .collect();
    if kids.is_empty() {
        return head;
    }
    let sep = if head.ends_with(':') { " " } else { ": " };
    format!("{head}{sep}{}", kids.join("; "))
}

/// The last section of this kind that has items; empty when none do.
fn pick_items(sections: &[Vec<Cow<'_, str>>]) -> Vec<RawItem> {
    let mut picked = Vec::new();
    for lines in sections {
        let items = parse_items(lines);
        if !items.is_empty() {
            picked = items;
        }
    }
    picked
}

/// The status block of an assistant reply, or `None` when it has no progress line and no section.
pub fn extract_status(markdown: &str) -> Option<Status> {
    let lines = unfenced(markdown);

    let mut progress_pct = None;
    let mut phase = None;
    for line in &lines {
        if let Some(m) = PROGRESS_RE.captures(line) {
            progress_pct = Some(js_round(number(&m[1])).clamp(0.0, 100.0) as i64);
            phase = Some(strip_markdown(&m[2]));
            break;
        }
    }

    let mut remaining: Vec<Vec<Cow<'_, str>>> = Vec::new();
    let mut waiting: Vec<Vec<Cow<'_, str>>> = Vec::new();
    let mut current: Option<(Kind, usize)> = None;
    for &line in &lines {
        if let Some((kind, rest)) = match_heading(line) {
            let sections = if kind == Kind::Remaining {
                &mut remaining
            } else {
                &mut waiting
            };
            let mut section = Vec::new();
            if !rest.is_empty() {
                section.push(Cow::Owned(format!("- {rest}")));
            }
            sections.push(section);
            current = Some((kind, sections.len() - 1));
            continue;
        }
        if ANY_HEADING.is_match(line) || THEMATIC_BREAK.is_match(line) || TABLE_ROW.is_match(line) {
            current = None;
            continue;
        }
        match current {
            Some((Kind::Remaining, i)) => remaining[i].push(Cow::Borrowed(line)),
            Some((Kind::Waiting, i)) => waiting[i].push(Cow::Borrowed(line)),
            None => {}
        }
    }

    if progress_pct.is_none() && remaining.is_empty() && waiting.is_empty() {
        return None;
    }

    let mut recommendations: Vec<String> = Vec::new();
    for line in &lines {
        if !RECOMMEND_RE.is_match(line) {
            continue;
        }
        let text = strip_markdown(&RECOMMEND_LEAD.replace(line, ""));
        if !text.is_empty() && !recommendations.contains(&text) {
            recommendations.push(text);
        }
        if recommendations.len() >= MAX_RECOMMENDATIONS {
            break;
        }
    }

    Some(Status {
        progress_pct,
        phase,
        remaining: pick_items(&remaining).iter().map(to_remaining).collect(),
        waiting: pick_items(&waiting).iter().map(to_waiting).collect(),
        recommendations,
    })
}
