//! Secret masking, a port of `src/security/mask.ts` that gives the same output for every input.
//!
//! The rules, their order and their labels are the TS ones. The TS patterns use look-around, which
//! the `regex` crate has not got, so each rule is a `regex` head plus hand-checked look-around:
//! - a one-character look-behind `(?<![C])` is consumed in the regex as `(?:^|[^C])` (first search)
//!   or `[^C]` (later searches, started one character early), and the match proper is group `m`;
//! - a one-character negative look-ahead that ends a pattern is consumed as `(?:[^C]|$)` after `m`;
//! - `(?!\[masked:)`, the value after a header and the private-key body are scanned by hand.
//!
//! Every head is deterministic (one way to match from a given start), so a failed hand check means
//! no match at that start, exactly as in JS, and the search goes on one character later. The regex
//! crate runs in linear time and the hand scans only move forward, so masking stays linear.
//!
//! JS translation: `\w` and `\d` are ASCII; `\s` is [`JS_SPACE_CLASS`]; the `i` flag (without `u`)
//! folds ASCII only, so case-insensitive words are written `(?i-u:...)`.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::{Captures, Regex};

use crate::text::{JS_SPACE_CLASS as WS, is_js_space, utf16_len};

const LABEL: &str = "[masked:";

/// ASCII `[\w-]`, `[\w.-]` and `\w` as class bodies.
const W: &str = r"A-Za-z0-9_";
const W_DASH: &str = r"A-Za-z0-9_\-";
const W_DOT_DASH: &str = r"A-Za-z0-9_.\-";

fn compile(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|e| panic!("mask pattern {pattern:?}: {e}"))
}

/// Finds rule candidates. The match proper (what JS reports as the match) is group `m`.
struct Scanner {
    /// Search from position 0, where the start of the text satisfies the look-behind.
    first: Regex,
    /// For a search from `pos > 0` in a rule with a one-character look-behind: the regex consumes
    /// the look-behind character, so the search starts at the character before `pos`.
    later: Option<Regex>,
}

impl Scanner {
    /// No one-character look-behind; `pattern` holds group `m` and any consumed context.
    fn plain(pattern: &str) -> Self {
        Self {
            first: compile(pattern),
            later: None,
        }
    }

    /// `(?<![not_before])` then `head` (which holds group `m`).
    fn behind(not_before: &str, head: &str) -> Self {
        Self {
            first: compile(&format!("(?:^|[^{not_before}]){head}")),
            later: Some(compile(&format!("[^{not_before}]{head}"))),
        }
    }

    /// The leftmost candidate whose match proper starts at or after `pos`.
    fn find<'h>(&self, s: &'h str, pos: usize) -> Option<Captures<'h>> {
        match &self.later {
            Some(later) if pos > 0 => later.captures_at(s, prev_boundary(s, pos)),
            _ => self.first.captures_at(s, pos),
        }
    }
}

fn prev_boundary(s: &str, pos: usize) -> usize {
    let mut i = pos - 1;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn next_boundary(s: &str, pos: usize) -> usize {
    pos + s[pos..].chars().next().map_or(1, char::len_utf8)
}

/// JS `text.replace(re, fn)` for a global `re`: `f` gets each candidate and returns the end of the
/// whole match and its replacement, or `None` when the hand-checked part fails there (then the next
/// candidate may start one character after it).
fn replace_matches<'a>(
    s: &'a str,
    scanner: &Scanner,
    mut f: impl FnMut(&Captures<'a>) -> Option<(usize, Cow<'a, str>)>,
) -> Cow<'a, str> {
    let mut out = String::new();
    let mut last = 0;
    let mut pos = 0;
    let mut replaced = false;
    while pos <= s.len() {
        let Some(caps) = scanner.find(s, pos) else {
            break;
        };
        let Some(m) = caps.name("m") else { break };
        match f(&caps) {
            Some((end, with)) => {
                out.push_str(&s[last..m.start()]);
                out.push_str(&with);
                last = end;
                pos = end;
                replaced = true;
            }
            None => pos = next_boundary(s, m.start()),
        }
    }
    if !replaced {
        return Cow::Borrowed(s);
    }
    out.push_str(&s[last..]);
    Cow::Owned(out)
}

/// What follows the regex head of a rule.
enum Tail {
    /// Nothing: the match is group `m`.
    None,
    /// `(?!\[masked:)` then a value: one `first` character, then `rest` characters (each also
    /// preceded by `(?!\[masked:)` when `label_stops`), at least `min` in all, then `=*` when
    /// `equals`, then the look-ahead `(?=@)` when `before_at`.
    Value {
        first: fn(char) -> bool,
        rest: fn(char) -> bool,
        label_stops: bool,
        min: usize,
        equals: bool,
        before_at: bool,
    },
    /// `(?:[^-]|-(?!----))*(?:-----END [A-Z ]*PRIVATE KEY-----)?`
    PrivateKey,
}

static PRIVATE_KEY_END: LazyLock<Regex> =
    LazyLock::new(|| compile(r"\A-----END [A-Z ]*PRIVATE KEY-----"));

impl Tail {
    /// End of the whole match whose head ends at `i`, or `None` when there is no match.
    fn end(&self, s: &str, i: usize) -> Option<usize> {
        match *self {
            Tail::None => Some(i),
            Tail::PrivateKey => {
                let body_end = s[i..].find("-----").map_or(s.len(), |k| i + k);
                Some(
                    PRIVATE_KEY_END
                        .find(&s[body_end..])
                        .map_or(body_end, |m| body_end + m.end()),
                )
            }
            Tail::Value {
                first,
                rest,
                label_stops,
                min,
                equals,
                before_at,
            } => {
                if s[i..].starts_with(LABEL) {
                    return None;
                }
                let mut chars = s[i..].char_indices();
                let (_, c) = chars.next()?;
                if !first(c) {
                    return None;
                }
                let mut count = 1;
                let mut j = s.len();
                for (k, c) in chars {
                    if !rest(c) || (label_stops && s[i + k..].starts_with(LABEL)) {
                        j = i + k;
                        break;
                    }
                    count += 1;
                }
                // The value characters are all ASCII where `min` > 1, so chars count UTF-16 units.
                if count < min {
                    return None;
                }
                if equals {
                    j += s[j..].len() - s[j..].trim_start_matches('=').len();
                }
                (!before_at || s[j..].starts_with('@')).then_some(j)
            }
        }
    }
}

struct Rule {
    scanner: Scanner,
    tail: Tail,
    label: &'static str,
    /// The TS replacement is `$1[masked:...]`: group 1 is the head (group `m` here).
    keep_head: bool,
}

fn rule(scanner: Scanner, label: &'static str) -> Rule {
    Rule {
        scanner,
        tail: Tail::None,
        label,
        keep_head: false,
    }
}

fn value_rule(scanner: Scanner, tail: Tail, label: &'static str) -> Rule {
    Rule {
        scanner,
        tail,
        label,
        keep_head: true,
    }
}

/// A value of one or more characters of `class`, `(?!\[masked:)` before the first only.
fn run_of(class: fn(char) -> bool) -> Tail {
    Tail::Value {
        first: class,
        rest: class,
        label_stops: false,
        min: 1,
        equals: false,
        before_at: false,
    }
}

static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    vec![
        Rule {
            scanner: Scanner::plain(r"(?P<m>-----BEGIN [A-Z ]*PRIVATE KEY-----)"),
            tail: Tail::PrivateKey,
            label: "private-key",
            keep_head: false,
        },
        // (?<=;[ \t]*): the ";" and blanks are consumed. Searching from `pos` is enough: a match
        // never ends inside a blank run that follows a ";", and a failed start is a letter.
        value_rule(
            Scanner::plain(r";[ \t]*(?P<m>(?i-u:password|pwd)[ \t]*=[ \t]*)"),
            Tail::Value {
                first: |c| c != ';' && !is_js_space(c),
                rest: |c| c != ';' && c != '\n',
                label_stops: true,
                min: 1,
                equals: false,
                before_at: false,
            },
            "conn-string",
        ),
        value_rule(
            Scanner::plain(r"(?P<m>AccountKey=|SharedAccessKey=|SharedAccessSignature=)"),
            run_of(|c| c != ';' && !is_js_space(c)),
            "azure-conn",
        ),
        value_rule(
            Scanner::plain(r"(?P<m>[?&]sig=)"),
            run_of(|c| !matches!(c, '&' | '"' | '\'') && !is_js_space(c)),
            "sas-sig",
        ),
        value_rule(
            Scanner::behind(
                r"A-Za-z0-9+.\-",
                &format!(r"(?P<m>[A-Za-z][A-Za-z0-9+.\-]*://[^:/{WS}@]*:)"),
            ),
            Tail::Value {
                first: |c| !matches!(c, '@' | '/') && !is_js_space(c),
                rest: |c| !matches!(c, '@' | '/') && !is_js_space(c),
                label_stops: false,
                min: 1,
                equals: false,
                before_at: true,
            },
            "url-cred",
        ),
        rule(
            Scanner::behind(
                W_DASH,
                &format!(r"(?P<m>eyJ[{W_DASH}]+\.[{W_DASH}]+\.[{W_DASH}]+)"),
            ),
            "jwt",
        ),
        rule(
            Scanner::plain(r"(?P<m>gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{60,})"),
            "github",
        ),
        rule(
            Scanner::plain(&format!(r"(?P<m>sk-ant-[{W_DASH}]{{20,}})")),
            "anthropic",
        ),
        rule(
            Scanner::plain(&format!(
                r"(?P<m>sk-(?:proj|svcacct|admin)-[{W_DASH}]{{20,}}|sk-[A-Za-z0-9]{{32,}})"
            )),
            "openai",
        ),
        rule(
            Scanner::plain(&format!(r"(?P<m>xox[abprs]-[{W_DASH}]{{10,}})")),
            "slack",
        ),
        rule(
            Scanner::plain(r"(?P<m>AKIA[0-9A-Z]{16})(?:[^0-9A-Z]|$)"),
            "aws",
        ),
        rule(
            Scanner::behind(
                r"A-Za-z0-9_:",
                &format!(r"(?P<m>[0-9]{{8,10}}:[{W_DASH}]{{35}})(?:[^{W_DASH}]|$)"),
            ),
            "telegram",
        ),
        rule(
            Scanner::behind(W, r"(?P<m>(?:sk|rk)_(?:live|test)_[A-Za-z0-9]{16,})"),
            "stripe",
        ),
        rule(
            Scanner::behind(
                W_DASH,
                &format!(r"(?P<m>AIza[{W_DASH}]{{35}})(?:[^{W_DASH}]|$)"),
            ),
            "google",
        ),
        rule(
            Scanner::behind(
                W_DOT_DASH,
                &format!(r"(?P<m>SG\.[{W_DASH}]{{22}}\.[{W_DASH}]{{43}})(?:[^{W_DASH}]|$)"),
            ),
            "sendgrid",
        ),
        rule(
            Scanner::behind(W, r"(?P<m>npm_[A-Za-z0-9]{36})(?:[^A-Za-z0-9]|$)"),
            "npm",
        ),
        rule(
            Scanner::behind(W_DASH, &format!(r"(?P<m>glpat-[{W_DASH}]{{20,}})")),
            "gitlab",
        ),
        rule(
            Scanner::behind(W_DASH, r"(?P<m>hf_[A-Za-z0-9]{30,})"),
            "huggingface",
        ),
        rule(
            Scanner::behind(W_DOT_DASH, &format!(r"(?P<m>hv[sbr]\.[{W_DASH}]{{24,}})")),
            "vault",
        ),
        rule(
            Scanner::plain(&format!(
                r"(?P<m>https://(?:hooks\.slack\.com/services/[A-Za-z0-9/]+|(?:discord|discordapp)\.com/api/webhooks/[0-9]+/[{W_DASH}]+))"
            )),
            "webhook",
        ),
        value_rule(
            Scanner::behind(
                W_DASH,
                r#"(?P<m>(?i-u:authorization)["']?[ \t]*:[ \t]*["']?(?i-u:bearer|basic|token)[ \t]+)"#,
            ),
            run_of(|c| !matches!(c, '"' | '\'') && !is_js_space(c)),
            "auth-header",
        ),
        value_rule(
            Scanner::behind(
                W_DASH,
                r#"(?P<m>(?i-u:x-api-key|api-key|ocp-apim-subscription-key)["']?[ \t]*:[ \t]*["']?)"#,
            ),
            run_of(|c| !matches!(c, '"' | '\'') && !is_js_space(c)),
            "api-key-header",
        ),
        // A bearer token outside an Authorization header (curl -H, logs): 20+ token characters.
        value_rule(
            Scanner::behind(W_DASH, r"(?P<m>Bearer[ \t]+)"),
            Tail::Value {
                first: is_bearer_char,
                rest: is_bearer_char,
                label_stops: false,
                min: 20,
                equals: true,
                before_at: false,
            },
            "bearer",
        ),
    ]
});

fn is_bearer_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '+' | '/' | '-')
}

fn apply_rule<'a>(s: &'a str, r: &Rule) -> Cow<'a, str> {
    replace_matches(s, &r.scanner, |caps| {
        let m = caps.name("m")?;
        let end = r.tail.end(s, m.end())?;
        let head = if r.keep_head { m.as_str() } else { "" };
        Some((end, Cow::Owned(format!("{head}{LABEL}{}]", r.label))))
    })
}

// ---- personal data (PII-02): a candidate is masked only when its check digits are valid ----

static CARD: LazyLock<Scanner> = LazyLock::new(|| {
    Scanner::behind(
        W_DOT_DASH,
        &format!(r"(?P<m>(?:[0-9][ \-]?){{12,18}}[0-9])(?:[^{W_DASH}]|$)"),
    )
});
static IBAN: LazyLock<Scanner> = LazyLock::new(|| {
    Scanner::behind(
        "A-Za-z0-9",
        r"(?P<m>[A-Z]{2}[0-9]{2}(?: ?[A-Z0-9]{4}){2,7}(?: ?[A-Z0-9]{1,3})?)(?:[^A-Za-z0-9]|$)",
    )
});

/// Card-network prefixes with the lengths each issues (Visa, Mastercard incl. 2-series, Amex,
/// Discover, UnionPay, JCB).
static CARD_SHAPES: LazyLock<Vec<(Regex, &'static [usize])>> = LazyLock::new(|| {
    const ANY: &[usize] = &[16, 17, 18, 19];
    vec![
        (compile(r"\A4"), &[16, 19]),
        (compile(r"\A5[1-5]"), &[16]),
        (
            compile(r"\A2(?:22[1-9]|2[3-9][0-9]|[3-6][0-9][0-9]|7[01][0-9]|720)"),
            &[16],
        ),
        (compile(r"\A3[47]"), &[15]),
        (compile(r"\A(?:6011|65|64[4-9])"), ANY),
        (compile(r"\A62"), ANY),
        (compile(r"\A35(?:2[89]|[3-8][0-9])"), ANY),
    ]
});

/// How a card is written inside a longer digit run: one unbroken group, groups of four (16 digits,
/// or 19 with a last group of 1 to 3), or Amex 4-6-5.
static CARD_WINDOW: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r"\A(?:[0-9]+|[0-9]{4}(?:[ \-][0-9]{4}){3}(?:[ \-][0-9]{1,3})?|[0-9]{4}[ \-][0-9]{6}[ \-][0-9]{5})\z",
    )
});

/// A card-network prefix and length, and a valid Luhn check.
fn is_card(candidate: &str) -> bool {
    let d: String = candidate
        .chars()
        .filter(|c| *c != ' ' && *c != '-')
        .collect();
    if !CARD_SHAPES
        .iter()
        .any(|(prefix, lengths)| prefix.is_match(&d) && lengths.contains(&d.len()))
    {
        return false;
    }
    let mut sum = 0u32;
    for (i, b) in d.bytes().rev().enumerate() {
        let mut n = u32::from(b.wrapping_sub(b'0'));
        if i % 2 == 1 {
            n *= 2;
            if n > 9 {
                n -= 9;
            }
        }
        sum += n;
    }
    sum.is_multiple_of(10)
}

/// Masks a CARD match (ASCII digits, spaces and dashes). When the whole match is not a card, tries
/// every card-shaped window of 15 to 19 digits that starts at the beginning or after a separator
/// and ends at the end or before one, longest first, and keeps the text around it.
fn mask_card(m: &str) -> Cow<'_, str> {
    if is_card(m) {
        return Cow::Borrowed("[masked:card]");
    }
    let b = m.as_bytes();
    let mut starts = vec![0];
    let mut ends = Vec::new();
    for (k, &c) in b.iter().enumerate().skip(1) {
        if c == b' ' || c == b'-' {
            ends.push(k);
            starts.push(k + 1);
        }
    }
    ends.push(b.len());
    if starts.len() == 1 {
        return Cow::Borrowed(m);
    }
    let mut digits_before = vec![0];
    for g in 0..starts.len() {
        digits_before.push(digits_before[g] + (ends[g] - starts[g]));
    }
    let mut best: Option<(usize, usize)> = None;
    for a in 0..starts.len() {
        for z in a..ends.len() {
            let digits = digits_before[z + 1] - digits_before[a];
            if !(15..=19).contains(&digits) {
                continue;
            }
            let (i, k) = (starts[a], ends[z]);
            if best.is_some_and(|(bi, bk)| k - i <= bk - bi) {
                continue;
            }
            let w = &m[i..k];
            if CARD_WINDOW.is_match(w) && is_card(w) {
                best = Some((i, k));
            }
        }
    }
    match best {
        None => Cow::Borrowed(m),
        Some((i, k)) => Cow::Owned(format!("{}[masked:card]{}", &m[..i], &m[k..])),
    }
}

/// ISO 13616 mod-97 check over the digit-expanded string, with an incremental remainder.
fn is_iban(candidate: &str) -> bool {
    let s: Vec<u8> = candidate.bytes().filter(|b| *b != b' ').collect();
    if !(15..=34).contains(&s.len()) {
        return false;
    }
    let mut rem = 0u32;
    let mut push = |digit: u32| rem = (rem * 10 + digit) % 97;
    for &c in s[4..].iter().chain(&s[..4]) {
        if c >= b'A' {
            let v = u32::from(c) - 55;
            push(v / 10);
            push(v % 10);
        } else {
            push(u32::from(c.wrapping_sub(b'0')));
        }
    }
    rem == 1
}

fn mask_pii(s: &str) -> Cow<'_, str> {
    let carded = replace_matches(s, &CARD, |caps| {
        let m = caps.name("m")?;
        Some((m.end(), mask_card(m.as_str())))
    });
    let ibans = replace_matches(&carded, &IBAN, |caps| {
        let m = caps.name("m")?;
        let with = if is_iban(m.as_str()) {
            Cow::Borrowed("[masked:iban]")
        } else {
            Cow::Owned(m.as_str().to_owned())
        };
        Some((m.end(), with))
    });
    match ibans {
        Cow::Owned(o) => Cow::Owned(o),
        Cow::Borrowed(_) => carded,
    }
}

// ---- key = value pairs ----

/// `(["']?[ \t]*[=:][ \t]*)`
const SEP: &str = r#"(?P<sep>["']?[ \t]*[=:][ \t]*)"#;

/// The identifier must END with the keyword (client_secret, db.password, AWS_SECRET_ACCESS_KEY
/// match; max_tokens, secretary do not).
static PASSWORD_KEY: LazyLock<Scanner> = LazyLock::new(|| {
    Scanner::behind(
        W_DOT_DASH,
        &format!(
            r"(?P<m>(?P<key>(?:[{W_DOT_DASH}]*[_.\-])?(?i-u:password|passwd|pwd|secret|token|api[_\-]?key|access[_\-]?key|(?:secret|signing|hmac|private|encryption|master)[_\-]?key)){SEP})"
        ),
    )
});
/// camelCase keys (accessToken, clientSecret, dbPassword); case-sensitive.
static CAMEL_KEY: LazyLock<Scanner> = LazyLock::new(|| {
    Scanner::behind(
        W_DOT_DASH,
        &format!(
            r"(?P<m>(?P<key>[{W_DOT_DASH}]*[a-z0-9](?:Password|Passwd|Pwd|Secret|Token|ApiKey|AccessKey|SecretKey|SigningKey|HmacKey|PrivateKey|EncryptionKey|MasterKey)){SEP})"
        ),
    )
});

static PWD_KEY: LazyLock<Regex> = LazyLock::new(|| compile(r"(?:^|[_.\-])(?i-u:pwd|passwd)\z"));
static CAMEL_PWD_KEY: LazyLock<Regex> = LazyLock::new(|| compile(r"[a-z0-9](?:Pwd|Passwd)\z"));
static PATHLIKE: LazyLock<Regex> = LazyLock::new(|| compile(r#"\A["']?(?:[/\\]|[A-Za-z]:\\)"#));

/// End of the value that starts at `i`, or `None` when there is none. The value is a quoted string
/// ("..." or '...' on one line, not starting with a label) or else the run of non-space characters
/// up to the first label, at least 6 UTF-16 units long.
fn value_end(s: &str, i: usize) -> Option<usize> {
    let b = s.as_bytes();
    if let Some(&q) = b.get(i)
        && (q == b'"' || q == b'\'')
        && !s[i + 1..].starts_with(LABEL)
    {
        let j = s[i + 1..]
            .find(|c: char| c == char::from(q) || c == '\n')
            .map_or(s.len(), |k| i + 1 + k);
        if b.get(j) == Some(&q) && j > i + 1 {
            return Some(j + 1);
        }
    }
    // Each step stops at whitespace, the end, or a "["; only a "[" that starts a label ends the value.
    let mut j = i;
    loop {
        j = s[j..]
            .find(|c: char| c == '[' || is_js_space(c))
            .map_or(s.len(), |k| j + k);
        if b.get(j) != Some(&b'[') || s[j..].starts_with(LABEL) {
            break;
        }
        j += 1;
    }
    (utf16_len(&s[i..j]) >= 6).then_some(j)
}

/// `pwd: /some/path` is a working directory, not a password.
fn is_pwd_path(key: &str, value: &str) -> bool {
    (PWD_KEY.is_match(key) || CAMEL_PWD_KEY.is_match(key)) && PATHLIKE.is_match(value)
}

/// Masks the value after every key that `keys` finds; keeps the key and separator.
fn mask_kv<'a>(text: &'a str, keys: &Scanner) -> Cow<'a, str> {
    replace_matches(text, keys, |caps| {
        let m = caps.name("m")?;
        let end = value_end(text, m.end())?;
        let key = caps.name("key")?.as_str();
        let sep = caps.name("sep")?.as_str();
        let with = if is_pwd_path(key, &text[m.end()..end]) {
            Cow::Borrowed(&text[m.start()..end])
        } else {
            Cow::Owned(format!("{key}{sep}[masked:password-kv]"))
        };
        Some((end, with))
    })
}

/// Masks secrets and personal data (cards, IBANs) in `text`, exactly as `maskSecrets` in TS.
/// Idempotent: a value never swallows an existing "[masked:...]" label.
pub fn mask_secrets(text: &str) -> String {
    let mut out = text.to_owned();
    for r in RULES.iter() {
        if let Cow::Owned(o) = apply_rule(&out, r) {
            out = o;
        }
    }
    if let Cow::Owned(o) = mask_pii(&out) {
        out = o;
    }
    if let Cow::Owned(o) = mask_kv(&out, &PASSWORD_KEY) {
        out = o;
    }
    if let Cow::Owned(o) = mask_kv(&out, &CAMEL_KEY) {
        out = o;
    }
    out
}

/// True when [`mask_secrets`] would change `text`.
pub fn looks_secret(text: &str) -> bool {
    mask_secrets(text) != text
}
