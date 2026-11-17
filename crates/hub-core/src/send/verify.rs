//! Delivery verification of relay sends (port of `src/send/verify.ts`).
//!
//! A send is "delivered" only on an exact SHA-256 match of the sent text in the recipient
//! sessionId's own transcript (a peer record from `session-hub-relay`, timestamp not before the
//! send started, minus 1 s of clock skew). Nothing else ever reports success.

use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::config::{
    DELIVERY_WINDOW_MS, EVAL_SESSION_PREFIX, MESSAGE_HEADER, RELAY_NAME, RETENTION_MS,
};
use crate::daemon::log::{LogValue, Logger};
use crate::ingest::records::Classified;
use crate::store::{SendRow, SendState, Store, StoreError};
use crate::text::is_js_space;

/// Records up to this much before `created_at` still count (clock skew between hub and recipient).
const SKEW_MS: i64 = 1000;
const WINDOW_MS: i64 = DELIVERY_WINDOW_MS as i64;
const RETENTION: i64 = RETENTION_MS as i64;

/// Detail of a send proved by a body the relay sent without the header.
pub const HEADERLESS_DETAIL: &str = "delivered without the Session Hub header";

/// Detail of a send proved by a body whose line break after the header the relay changed.
pub const REFLOWED_DETAIL: &str = "delivered; the relay changed the line break after the header";

/// The exact text the relay puts on SendMessage: header, blank line, the owner's text unchanged.
pub fn build_message(text: &str) -> String {
    format!("{MESSAGE_HEADER}\n\n{text}")
}

/// CRLF -> LF, then trailing whitespace trimmed (end of string only, JS `trimEnd` set).
pub fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n")
        .trim_end_matches(is_js_space)
        .to_string()
}

/// Lowercase hex SHA-256 of the UTF-8 bytes.
pub fn sha256_hex(text: &str) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(text.as_bytes());
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 15)]));
    }
    out
}

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Reads exactly `n` ASCII digits at `*i`.
fn digits(b: &[u8], i: &mut usize, n: usize) -> Option<i64> {
    let s = b.get(*i..*i + n)?;
    if !s.iter().all(u8::is_ascii_digit) {
        return None;
    }
    *i += n;
    Some(s.iter().fold(0, |a, d| a * 10 + i64::from(d - b'0')))
}

fn eat(b: &[u8], i: &mut usize, c: u8) -> bool {
    if b.get(*i) == Some(&c) {
        *i += 1;
        true
    } else {
        false
    }
}

/// `Date.parse(s)` in ms since the epoch for the ECMAScript date-time string format
/// (`YYYY`, `YYYY-MM`, `YYYY-MM-DD`, optional `THH:mm[:ss[.sss]]`, optional `Z` or `+HH:mm`, and
/// the expanded `+/-YYYYYY` year), `None` where `Date.parse` gives NaN. A date-time without an
/// offset is read as UTC (JS reads it as local time; Claude Code always writes `Z`). The legacy
/// free-form formats V8 and JSC also accept are not supported and give `None`.
pub fn parse_js_date(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let mut i = 0;
    let year = match b.first()? {
        sign @ (b'+' | b'-') => {
            i = 1;
            let y = digits(b, &mut i, 6)?;
            if *sign == b'-' {
                if y == 0 {
                    return None;
                }
                -y
            } else {
                y
            }
        }
        _ => digits(b, &mut i, 4)?,
    };
    let (mut month, mut day) = (1, 1);
    if eat(b, &mut i, b'-') {
        month = digits(b, &mut i, 2)?;
        if eat(b, &mut i, b'-') {
            day = digits(b, &mut i, 2)?;
        }
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (mut h, mut min, mut sec, mut ms, mut offset_min) = (0, 0, 0, 0, 0);
    if eat(b, &mut i, b'T') {
        h = digits(b, &mut i, 2)?;
        if !eat(b, &mut i, b':') {
            return None;
        }
        min = digits(b, &mut i, 2)?;
        if eat(b, &mut i, b':') {
            sec = digits(b, &mut i, 2)?;
            if eat(b, &mut i, b'.') {
                let start = i;
                while b.get(i).is_some_and(u8::is_ascii_digit) {
                    i += 1;
                }
                if i == start {
                    return None;
                }
                let mut frac = 0;
                for k in 0..3 {
                    frac = frac * 10
                        + b.get(start + k)
                            .filter(|_| start + k < i)
                            .map_or(0, |d| i64::from(d - b'0'));
                }
                ms = frac;
            }
        }
        if h > 24 || min > 59 || sec > 59 || (h == 24 && (min, sec, ms) != (0, 0, 0)) {
            return None;
        }
        match b.get(i) {
            Some(b'Z') => i += 1,
            Some(&sign @ (b'+' | b'-')) => {
                i += 1;
                let oh = digits(b, &mut i, 2)?;
                if !eat(b, &mut i, b':') {
                    return None;
                }
                let om = digits(b, &mut i, 2)?;
                if oh > 23 || om > 59 {
                    return None;
                }
                offset_min = (oh * 60 + om) * if sign == b'-' { -1 } else { 1 };
            }
            _ => {}
        }
    }
    if i != b.len() {
        return None;
    }
    let ms_total = ((days_from_civil(year, month, day) * 24 + h) * 60 + min - offset_min) * 60_000
        + sec * 1000
        + ms;
    // Date.parse yields NaN beyond +/-8.64e15 ms.
    (ms_total.abs() <= 8_640_000_000_000_000).then_some(ms_total)
}

/// A record counts when its timestamp parses and is not more than `SKEW_MS` before `created_at`.
fn after(ts: &str, created_at: i64) -> bool {
    parse_js_date(ts).is_some_and(|t| t >= created_at - SKEW_MS)
}

enum Match {
    Full,
    Headerless,
    Reflowed,
}

/// Length of the whitespace between the header line and the text: at least one line break,
/// nothing else (JS `^(?:[ \t]*\r?\n)+`). `None` when there is no line break.
fn header_break_len(after: &str) -> Option<usize> {
    let b = after.as_bytes();
    let mut end = 0;
    loop {
        let mut j = end;
        while matches!(b.get(j), Some(b' ' | b'\t')) {
            j += 1;
        }
        if b.get(j) == Some(&b'\r') {
            j += 1;
        }
        if b.get(j) == Some(&b'\n') {
            end = j + 1;
        } else {
            break;
        }
    }
    (end > 0).then_some(end)
}

/// The send hashes a relay body can prove (match and the stray check use the same three).
struct BodyHashes {
    /// The body as the whole message.
    full: String,
    /// When the body lacks the header, the body with the header put back.
    headerless: Option<String>,
    /// When the body has the header, the text after a different line break, put back after the
    /// header with the standard blank line.
    reflowed: Option<String>,
}

fn body_hashes(raw: &str) -> BodyHashes {
    let body = normalize(raw);
    let Some(after) = body.strip_prefix(MESSAGE_HEADER) else {
        return BodyHashes {
            full: sha256_hex(&body),
            headerless: Some(sha256_hex(&normalize(&build_message(&body)))),
            reflowed: None,
        };
    };
    // The header with the owner's text after a different line break (seen live: one instead of a
    // blank line). The text itself must still match exactly; only the break is put back.
    let reflowed = header_break_len(after)
        .map(|n| normalize(&build_message(&after[n..])))
        .filter(|r| *r != body)
        .map(|r| sha256_hex(&r));
    BodyHashes {
        full: sha256_hex(&body),
        headerless: None,
        reflowed,
    }
}

/// How a record proves this send: `Full` when its body is the whole message, `Headerless` when the
/// relay model dropped the header line (it sometimes does) and the body is exactly the owner's
/// text, `Reflowed` when only the line break between the header line and the owner's text differs.
/// The send row keeps only the hash of the whole message, so the header is put back first.
fn match_record(send: &SendRow, r: &Classified) -> Option<Match> {
    let Classified::PeerMessage {
        ts,
        from_name,
        body,
        ..
    } = r
    else {
        return None;
    };
    if from_name != RELAY_NAME || !after(ts, send.created_at) {
        return None;
    }
    let h = body_hashes(body);
    if h.full == send.sha256 {
        Some(Match::Full)
    } else if h.headerless.as_deref() == Some(send.sha256.as_str()) {
        Some(Match::Headerless)
    } else if h.reflowed.as_deref() == Some(send.sha256.as_str()) {
        Some(Match::Reflowed)
    } else {
        None
    }
}

/// Index of the record that proves this send arrived. Records must be the target's own.
fn delivery_index(send: &SendRow, records: &[&Classified]) -> Option<usize> {
    records.iter().position(|r| match_record(send, r).is_some())
}

/// How the first proving record matched (`"full"`, `"headerless"` or `"reflowed"`), or `None`
/// when none proves the send.
pub fn delivery_match(send: &SendRow, records: &[Classified]) -> Option<&'static str> {
    records
        .iter()
        .find_map(|r| match_record(send, r))
        .map(|m| match m {
            Match::Full => "full",
            Match::Headerless => "headerless",
            Match::Reflowed => "reflowed",
        })
}

fn check_refs(send: &SendRow, records: &[&Classified], now: i64) -> SendState {
    if matches!(send.state, SendState::Delivered | SendState::Expired) {
        return send.state;
    }
    let in_window = now - send.created_at <= WINDOW_MS;
    if send.state == SendState::Failed {
        return if in_window && delivery_index(send, records).is_some() {
            SendState::Delivered
        } else {
            SendState::Failed
        };
    }
    if delivery_index(send, records).is_some() {
        return SendState::Delivered;
    }
    if send.state == SendState::Pending
        && records.iter().any(|r| {
            matches!(r, Classified::HeldNotice { ts, peer_name, .. }
                if peer_name == RELAY_NAME && after(ts, send.created_at))
        })
    {
        return SendState::Held;
    }
    if !in_window {
        return SendState::Expired;
    }
    send.state
}

/// New state of a send given records from the target sessionId's transcript only.
/// delivered > held > expired > unchanged. delivered and expired never change. failed changes
/// only to delivered, on an exact match while inside `DELIVERY_WINDOW_MS` (the relay reported
/// failure or timed out, but the message did arrive).
pub fn check_delivery(send: &SendRow, records: &[Classified], now: i64) -> SendState {
    let refs: Vec<&Classified> = records.iter().collect();
    check_refs(send, &refs, now)
}

fn str_field(k: &'static str, v: &str) -> (&'static str, LogValue) {
    (k, LogValue::Str(v.to_string()))
}

/// Flags every relay message in these records whose text matches no send made to this session
/// within retention, in any state (LLM-01): the relay model wrote to a session or a text the owner
/// did not send. Counted in meta `relay_stray_count`, with the time of the last one in
/// `relay_stray_last`; the text is never stored or logged. The relay eval's own fixture sessions
/// (`EVAL_SESSION_PREFIX`) receive relays the hub did not send, by design, and are skipped.
fn report_strays(
    store: &Store,
    log: &dyn Logger,
    session_id: &str,
    records: &[Classified],
    now: i64,
) -> Result<(), StoreError> {
    let relay: Vec<(&str, &str)> = records
        .iter()
        .filter_map(|r| match r {
            Classified::PeerMessage {
                from_name,
                ts,
                body,
                ..
            } if from_name == RELAY_NAME => Some((ts.as_str(), body.as_str())),
            _ => None,
        })
        .collect();
    if relay.is_empty() {
        return Ok(());
    }
    if store
        .get_session(session_id)?
        .is_some_and(|s| s.name.starts_with(EVAL_SESSION_PREFIX))
    {
        return Ok(());
    }
    let known = store.send_hashes_since(session_id, now - RETENTION)?;
    for (ts, body) in relay {
        let h = body_hashes(body);
        if known.contains(&h.full)
            || h.headerless.is_some_and(|x| known.contains(&x))
            || h.reflowed.is_some_and(|x| known.contains(&x))
        {
            continue;
        }
        // Older than retention: its send row may be purged (a transcript read again from the start).
        match parse_js_date(ts) {
            Some(t) if t >= now - RETENTION => {}
            _ => continue,
        }
        store.tx(|| {
            let n = store
                .get_meta("relay_stray_count")?
                .and_then(|v| js_number(&v))
                .unwrap_or(0);
            store.set_meta("relay_stray_count", Some(&(n + 1).to_string()))?;
            store.set_meta("relay_stray_last", Some(&now.to_string()))
        })?;
        log.warn(
            "relay_stray",
            vec![str_field("sessionId", session_id), str_field("ts", ts)],
        );
    }
    Ok(())
}

/// `Number(v)` for a stored counter, `None` when not a finite number (TS then counts from 0).
fn js_number(v: &str) -> Option<i64> {
    let t = v.trim_matches(is_js_space);
    if t.is_empty() {
        return Some(0);
    }
    t.parse::<f64>()
        .ok()
        .filter(|n| n.is_finite())
        .map(|n| n as i64)
}

/// Returns the per-pass hook the daemon calls with a session's new records. Sends are checked
/// oldest first (pending/held, plus failed ones still inside the window), and a delivery record
/// proves at most one send, so two overridden identical messages need two records. A held-notice
/// names no message, so it applies only to the session's newest pending send. Every change is a
/// conditional store transition, so a concurrent writer can never undo delivered. The store is
/// passed per call (the collector thread owns the only `Store`). A store error ends the check
/// and is returned, as TS throws it: the collector logs it as `on_records_failed` and counts it.
pub fn delivery_checker(
    now: Arc<dyn Fn() -> i64 + Send + Sync>,
    log: Arc<dyn Logger>,
) -> impl FnMut(&Store, &str, &[Classified]) -> Result<(), StoreError> {
    move |store, session_id, records| {
        let t = now();
        report_strays(store, log.as_ref(), session_id, records, t)?;
        verify_sends(store, session_id, records, t)
    }
}

fn verify_sends(
    store: &Store,
    session_id: &str,
    records: &[Classified],
    t: i64,
) -> Result<(), StoreError> {
    let sends = store.sends_to_verify(session_id, t - WINDOW_MS)?;
    let newest_pending = sends
        .iter()
        .rfind(|s| s.state == SendState::Pending)
        .and_then(|s| s.id);
    let mut available: Vec<&Classified> = records.iter().collect();
    for s in &sends {
        let Some(id) = s.id else { continue };
        let idx = delivery_index(s, &available);
        let next = if Some(id) == newest_pending {
            check_refs(s, &available, t)
        } else {
            let no_held: Vec<&Classified> = available
                .iter()
                .copied()
                .filter(|r| !matches!(r, Classified::HeldNotice { .. }))
                .collect();
            check_refs(s, &no_held, t)
        };
        let how = if next == SendState::Delivered {
            idx.and_then(|i| match_record(s, available[i]))
        } else {
            None
        };
        if next == SendState::Delivered
            && let Some(i) = idx
        {
            available.remove(i);
        }
        // The store clears a failed send's detail when it moves to delivered; a body without the
        // header, or with another line break after it, is noted instead.
        if next != s.state {
            let detail = match how {
                Some(Match::Headerless) => Some(Some(HEADERLESS_DETAIL)),
                Some(Match::Reflowed) => Some(Some(REFLOWED_DETAIL)),
                _ => None,
            };
            store.transition_send(id, next, detail)?;
        }
    }
    Ok(())
}

/// Marks pending/held sends older than the delivery window expired. Returns how many. The first
/// store error aborts and is returned (TS throws); the caller logs it.
pub fn expire_sends(store: &Store, now: i64) -> Result<usize, StoreError> {
    let mut n = 0;
    for s in store.open_sends(None)? {
        if let Some(id) = s.id
            && now - s.created_at > WINDOW_MS
            && store.transition_send(id, SendState::Expired, None)?
        {
            n += 1;
        }
    }
    Ok(n)
}
