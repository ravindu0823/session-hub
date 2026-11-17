//! Which "Waiting on you" IDs the owner has answered, a port of `src/status/answered.ts` that gives
//! the same output for every input. The rules are documented there.
//!
//! JS translation: `ID_TOKEN` has a one-character look-behind `(?<![A-Za-z0-9-])`, checked by hand
//! before each candidate start, and a one-character negative look-ahead `(?![A-Za-z0-9])` at its
//! end, consumed as `(?:[^A-Za-z0-9]|$)` (same capture: nothing follows it; a token never starts
//! on the consumed character, so the scan resumes right after the ID like JS `lastIndex`).
//! `\band` in `SEPARATOR` is `(?:^|[^A-Za-z0-9_])and` (an unanchored test, so consuming the
//! character before "and" does not change the result). `(?<=\d)-` in the split is scanned by hand.

use std::collections::{BTreeSet, HashSet};
use std::sync::LazyLock;

use regex::Regex;

use super::extract::js_regex;
use crate::text::{clip_utf16, is_js_space, split_js_lines, utf16_len};

static ID_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    js_regex(r"^((?:[A-Z][A-Z0-9]{0,7}-)*[A-Z][A-Z0-9]{0,6}[0-9])(?:[^A-Za-z0-9]|$)")
});
/// What may come before an answering ID: line start plus markers, or a separator after an answer.
static LEAD: LazyLock<Regex> =
    LazyLock::new(|| js_regex(r"^[\s>*_`(#\-\x{2022}]*(?:[0-9]+[.)]\s*)?$"));
static SEPARATOR: LazyLock<Regex> =
    LazyLock::new(|| js_regex(r"(?:[/,;&]|(?:^|[^A-Za-z0-9_])and)[\s*_`(]*$"));
static DASH: LazyLock<Regex> = LazyLock::new(|| js_regex(r"^\s*[\-\x{2013}\x{2014}]\s*$"));

/// Text scanned per message and longest token tried as a range: bounds the work on hostile input.
const MAX_SCAN_CHARS: usize = 20_000;
const MAX_TOKEN_CHARS: usize = 40;

/// JS `id.replace(/\d+$/, "")`.
fn stem(id: &str) -> &str {
    id.trim_end_matches(|c: char| c.is_ascii_digit())
}

/// JS `Number(/\d+$/.exec(id)?.[0] ?? NaN)`.
fn num(id: &str) -> f64 {
    id[stem(id).len()..].parse().unwrap_or(f64::NAN)
}

/// An ID token of a line: its start (byte offset) and text.
struct Token<'a> {
    at: usize,
    text: &'a str,
}

/// JS `[...line.matchAll(ID_TOKEN)]`.
fn tokens(line: &str) -> Vec<Token<'_>> {
    let mut out = Vec::new();
    let mut prev: Option<char> = None;
    let mut iter = line.char_indices();
    while let Some((at, c)) = iter.next() {
        let behind_ok = !prev.is_some_and(|p| p.is_ascii_alphanumeric() || p == '-');
        prev = Some(c);
        if !c.is_ascii_uppercase() || !behind_ok {
            continue;
        }
        let Some(m) = ID_TOKEN.captures(&line[at..]).and_then(|caps| caps.get(1)) else {
            continue;
        };
        let text = m.as_str();
        out.push(Token { at, text });
        // Resume after the ID; its last character (a digit) is the next look-behind character.
        let end = at + text.len();
        while iter.offset() < end {
            prev = iter.next().map(|(_, c)| c);
        }
    }
    out
}

/// "XY-Q1-Q3" or "XY-Q1-XY-Q3": the two ends when the second has the first one's stem or its last
/// segment and a higher number; else `None`.
fn split_joined_range(token: &str) -> Option<(&str, String)> {
    for (i, _) in token.match_indices('-') {
        let first = &token[..i];
        let mut second = token[i + 1..].to_string();
        if !first.ends_with(|c: char| c.is_ascii_digit())
            || !second.starts_with(|c: char| c.is_ascii_uppercase())
        {
            continue;
        }
        let first_stem = stem(first);
        if first_stem
            .strip_suffix(stem(&second))
            .is_some_and(|head| head.ends_with('-'))
        {
            // JS `stem(first) + String(num(second))`: the digits of a token are at most 7, so the
            // number is an exact integer and prints without leading zeros.
            second = format!("{first_stem}{}", num(&second));
        }
        if first_stem == stem(&second) && num(&second) > num(first) {
            return Some((first, second));
        }
    }
    None
}

/// JS `token.split(/(?<=\d)-/)`.
fn parts(token: &str) -> impl Iterator<Item = &str> {
    let bytes = token.as_bytes();
    let mut start = 0;
    let mut cuts = token
        .match_indices('-')
        .filter(|&(i, _)| i > 0 && bytes[i - 1].is_ascii_digit())
        .map(|(i, _)| i)
        .chain([token.len()]);
    std::iter::from_fn(move || {
        let cut = cuts.next()?;
        let part = &token[start..cut];
        start = cut + 1;
        Some(part)
    })
}

/// Whether a line of a clipped message ends with "?" (JS `line.trimEnd().endsWith("?")`).
fn is_question(line: &str) -> bool {
    line.trim_end_matches(is_js_space).ends_with('?')
}

/// The listed IDs that the owner's messages `texts` answer.
pub fn answered_ids(listed: &[String], texts: &[String]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if listed.is_empty() {
        return out;
    }
    let on_list: HashSet<&str> = listed.iter().map(String::as_str).collect();
    let mark_range = |out: &mut BTreeSet<String>, from: &str, to: &str| {
        let (lo, hi) = (num(from), num(to));
        for id in listed {
            let n = num(id);
            if stem(id) == stem(from) && n >= lo && n <= hi {
                out.insert(id.clone());
            }
        }
    };
    for text in texts {
        let clipped = clip_utf16(text, MAX_SCAN_CHARS);
        // JS `text.slice(0, 20000)` can end in the first half of a surrogate pair, which
        // `clip_utf16` drops: then the last line ends in that half, never in "?".
        let cut_pair = utf16_len(clipped) < MAX_SCAN_CHARS && clipped.len() < text.len();
        let lines: Vec<&str> = split_js_lines(clipped).collect();
        for (n, &line) in lines.iter().enumerate() {
            let last = n + 1 == lines.len();
            if is_question(line) && !(last && cut_pair) {
                continue;
            }
            let tokens = tokens(line);
            let mut end = 0;
            let mut answering = false;
            let mut i = 0;
            while i < tokens.len() {
                let Token { at, text: token } = tokens[i];
                let before = &line[end..at];
                end = at + token.len();
                // Only the first token of a line may stand at its start; later ones need a separator.
                answering = if i == 0 {
                    LEAD.is_match(before)
                } else {
                    answering && SEPARATOR.is_match(before)
                };
                if !answering {
                    if i == 0 {
                        break;
                    }
                    i += 1;
                    continue;
                }
                if on_list.contains(token) {
                    out.insert(token.to_string());
                }
                if token.len() <= MAX_TOKEN_CHARS {
                    if let Some((from, to)) = split_joined_range(token) {
                        mark_range(&mut out, from, &to);
                    } else {
                        // "Q14-Q6", "AB7-Q7": not a range, but each part that is an ID is named.
                        for part in parts(token).filter(|p| on_list.contains(p)) {
                            out.insert(part.to_string());
                        }
                    }
                }
                // "AB7 - AB9" / "AB7–AB9": the next token, with only a dash between, closes a range.
                if let Some(next) = tokens.get(i + 1)
                    && DASH.is_match(&line[end..next.at])
                    && stem(next.text) == stem(token)
                    && num(next.text) > num(token)
                {
                    mark_range(&mut out, token, next.text);
                    end = next.at + next.text.len();
                    i += 1;
                }
                i += 1;
            }
        }
    }
    out
}
