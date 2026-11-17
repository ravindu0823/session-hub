//! Length helpers: the TypeScript original counts string lengths in UTF-16 code units.

/// Length of `s` in UTF-16 code units (JS `.length`).
pub fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Longest prefix of `s` that is at most `max_units` UTF-16 units and ends on a char boundary.
pub fn clip_utf16(s: &str, max_units: usize) -> &str {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        units += c.len_utf16();
        if units > max_units {
            return &s[..i];
        }
    }
    s
}

/// Body of a regex character class equal to JS `\s` (JavaScriptCore's list). Rust's `\s` differs:
/// it has U+0085 and lacks U+FEFF.
pub const JS_SPACE_CLASS: &str = r"\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";

/// JS `\s` for one character (see [`JS_SPACE_CLASS`]).
pub fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// JS `s.split(/\r?\n/)`: a `\r` is dropped only before a `\n`, so a last line keeps its own.
pub fn split_js_lines(s: &str) -> impl Iterator<Item = &str> {
    let mut pieces = s.split('\n').peekable();
    std::iter::from_fn(move || {
        let piece = pieces.next()?;
        Some(if pieces.peek().is_some() {
            piece.strip_suffix('\r').unwrap_or(piece)
        } else {
            piece
        })
    })
}
