//! Terminal-safe text, a port of `src/tui/sanitize.ts`. Everything the TUI prints from the store and
//! everything typed or pasted into the Send pane passes through here, so no stored or pasted byte
//! can move the cursor, set the window title, write the clipboard (OSC 52) or draw a fake line.

use std::sync::LazyLock;

use regex::{NoExpand, Regex};

/// OSC plus the other string controls (DCS, SOS, PM, APC), ended by BEL or ST.
static STRING_SEQ: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\x1B[\]PX^_][^\x07\x1B]*(?:\x07|\x1B\\)").expect("STRING_SEQ pattern")
});
static CSI: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\x1B\[[\x30-\x3F]*[\x20-\x2F]*[\x40-\x7E]").expect("CSI pattern")
});
/// What Ink leaves of an unrecognised escape sequence after it drops the ESC: "[200~", "[31m".
static CSI_TAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\A\[[\x30-\x3F]+[\x20-\x2F]*[\x40-\x7E]\z").expect("CSI_TAIL pattern")
});

/// JS `/\u001B[\s\S]?/g`: any other ESC-introduced pair, or a lone ESC at the end. JS removes one
/// UTF-16 unit after the ESC; before an astral character that leaves its low surrogate, which the
/// terminal receives as U+FFFD, so U+FFFD stands in for it here.
fn drop_other_esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1B}' {
            out.push(c);
        } else if chars.next().is_some_and(|n| n.len_utf16() == 2) {
            out.push('\u{FFFD}');
        }
    }
    out
}

/// Tab to two spaces; drops C0 controls except `\n`, DEL, C1 controls, and the bidi embeddings,
/// overrides and isolates that can reorder what a line appears to say.
fn drop_controls(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\t' => out.push_str("  "),
            '\n' => out.push('\n'),
            '\u{00}'..='\u{1F}'
            | '\u{7F}'..='\u{9F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}' => {}
            _ => out.push(c),
        }
    }
    out
}

/// Strips terminal control sequences and control characters; keeps `\n`, maps `\t` to two spaces.
pub fn clean_text(s: &str) -> String {
    let s = s.replace("\r\n", "\n");
    let s = STRING_SEQ.replace_all(&s, NoExpand(""));
    let s = CSI.replace_all(&s, NoExpand(""));
    drop_controls(&drop_other_esc(&s))
}

/// One typed or pasted chunk for a text input. `meta` is Ink's flag for ESC-prefixed input
/// (Alt+key and unrecognised escape tails); those never become text. A lone or CRLF line break
/// inside a pasted chunk becomes `\n`.
pub fn clean_input(chunk: &str, meta: bool) -> String {
    if meta || CSI_TAIL.is_match(chunk) {
        return String::new();
    }
    clean_text(&chunk.replace("\r\n", "\n").replace('\r', "\n"))
}
