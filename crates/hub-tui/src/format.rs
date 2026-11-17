//! Port of `src/tui/format.ts`: short ages, local clock times, labels and the hard wrap.
use hub_core::security::clean_text;
use hub_core::store::{LiveStatus, Role};
use hub_core::text::clip_utf16;

/// Short age: 45s, 4m, 3h, 2d. Negative input (clock skew) reads as 0s.
pub fn ago(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    if s < 60 {
        return format!("{s}s");
    }
    let m = s / 60;
    if m < 60 {
        return format!("{m}m");
    }
    let h = m / 60;
    if h < 48 {
        return format!("{h}h");
    }
    format!("{}d", h / 24)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Year, month, day of a day count since 1970-01-01.
#[cfg_attr(not(windows), allow(dead_code))]
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// A parsed ISO timestamp. `local_wall`: written without an offset, so a wall-clock time in the
/// local zone (JS `Date.parse` reads those as local time); `ms` then holds the wall clock as if UTC.
struct Parsed {
    ms: i64,
    local_wall: bool,
}

fn digits(b: &[u8], from: usize, n: usize) -> Option<i64> {
    let part = b.get(from..from + n)?;
    part.iter()
        .all(u8::is_ascii_digit)
        .then(|| part.iter().fold(0, |a, d| a * 10 + i64::from(d - b'0')))
}

/// The ISO 8601 forms JS `Date.parse` takes: `YYYY[-MM[-DD]][THH:MM[:SS[.fff]][Z|+HH:MM]]`.
fn parse_iso(s: &str) -> Option<Parsed> {
    let b = s.as_bytes();
    let year = digits(b, 0, 4)?;
    let mut i = 4;
    let (mut month, mut day) = (1, 1);
    if b.get(i) == Some(&b'-') {
        month = digits(b, i + 1, 2)?;
        i += 3;
        if b.get(i) == Some(&b'-') {
            day = digits(b, i + 1, 2)?;
            i += 3;
        }
    }
    if !(1..=12).contains(&month) || !(1..=days_in_month(year, month)).contains(&day) {
        return None;
    }
    let date_ms = days_from_civil(year, month, day) * 86_400_000;
    if i == b.len() {
        return Some(Parsed {
            ms: date_ms,
            local_wall: false,
        });
    }
    if b[i] != b'T' {
        return None;
    }
    let (h, min) = (digits(b, i + 1, 2)?, digits(b, i + 4, 2)?);
    if b.get(i + 3) != Some(&b':') {
        return None;
    }
    i += 6;
    let (mut sec, mut frac) = (0, 0);
    if b.get(i) == Some(&b':') {
        sec = digits(b, i + 1, 2)?;
        i += 3;
        if b.get(i) == Some(&b'.') {
            let start = i + 1;
            let mut end = start;
            while b.get(end).is_some_and(u8::is_ascii_digit) {
                end += 1;
            }
            if end == start {
                return None;
            }
            let three: String = s[start..end].chars().chain("00".chars()).take(3).collect();
            frac = three.parse::<i64>().ok()?;
            i = end;
        }
    }
    // 24:00:00 is the end of the day; nothing else may exceed 23:59:59.
    if min > 59 || sec > 59 || h > 24 || (h == 24 && (min, sec, frac) != (0, 0, 0)) {
        return None;
    }
    let wall = date_ms + ((h * 60 + min) * 60 + sec) * 1000 + frac;
    let (offset_min, local_wall) = match &b[i..] {
        [] => (0, true),
        [b'Z'] => (0, false),
        [sign @ (b'+' | b'-'), rest @ ..] if rest.len() == 5 && rest[2] == b':' => {
            let (oh, om) = (digits(rest, 0, 2)?, digits(rest, 3, 2)?);
            if oh > 23 || om > 59 {
                return None;
            }
            let v = oh * 60 + om;
            (if *sign == b'-' { -v } else { v }, false)
        }
        _ => return None,
    };
    Some(Parsed {
        ms: wall - offset_min * 60_000,
        local_wall,
    })
}

/// JS `Date.parse` of an ISO timestamp, in epoch ms; None when it is not one.
pub fn parse_ts(ts: &str) -> Option<i64> {
    parse_iso(ts).map(|p| p.ms)
}

#[cfg(unix)]
fn local_hour_min(epoch_ms: i64) -> Option<(i64, i64)> {
    use std::ffi::{c_char, c_int, c_long};
    // The first nine fields are the same on every Unix; glibc and the BSDs add gmtoff and zone
    // after them. The padding keeps `localtime_r` inside our buffer whatever else a libc appends.
    #[repr(C)]
    #[allow(dead_code)]
    struct Tm {
        sec: c_int,
        min: c_int,
        hour: c_int,
        mday: c_int,
        mon: c_int,
        year: c_int,
        wday: c_int,
        yday: c_int,
        isdst: c_int,
        gmtoff: c_long,
        zone: *const c_char,
        pad: [u64; 4],
    }
    unsafe extern "C" {
        fn localtime_r(t: *const c_long, out: *mut Tm) -> *mut Tm;
    }
    let t = c_long::try_from(epoch_ms.div_euclid(1000)).ok()?;
    let mut tm = Tm {
        sec: 0,
        min: 0,
        hour: 0,
        mday: 0,
        mon: 0,
        year: 0,
        wday: 0,
        yday: 0,
        isdst: 0,
        gmtoff: 0,
        zone: std::ptr::null(),
        pad: [0; 4],
    };
    // SAFETY: `localtime_r` reads one time_t and writes a `struct tm` into the buffer we give it,
    // which is at least as large as every libc's; it is thread-safe, unlike `localtime`.
    let r = unsafe { localtime_r(&t, &mut tm) };
    (!r.is_null()).then_some((i64::from(tm.hour), i64::from(tm.min)))
}

#[cfg(windows)]
fn local_hour_min(epoch_ms: i64) -> Option<(i64, i64)> {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::Time::SystemTimeToTzSpecificLocalTime;
    let secs = epoch_ms.div_euclid(1000);
    let (y, mo, d) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    let utc = SYSTEMTIME {
        wYear: u16::try_from(y).ok()?,
        wMonth: mo as u16,
        wDayOfWeek: 0,
        wDay: d as u16,
        wHour: (rem / 3600) as u16,
        wMinute: (rem % 3600 / 60) as u16,
        wSecond: (rem % 60) as u16,
        wMilliseconds: 0,
    };
    let mut local = utc;
    // SAFETY: both pointers name live SYSTEMTIMEs; a null zone means the current time zone.
    let ok = unsafe { SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) };
    (ok != 0).then_some((i64::from(local.wHour), i64::from(local.wMinute)))
}

/// Local HH:MM of an ISO timestamp; "??:??" when unparseable.
pub fn hhmm(ts: &str) -> String {
    let Some(p) = parse_iso(ts) else {
        return "??:??".into();
    };
    let (h, m) = if p.local_wall {
        (
            p.ms.rem_euclid(86_400_000) / 3_600_000,
            p.ms.rem_euclid(3_600_000) / 60_000,
        )
    } else {
        match local_hour_min(p.ms) {
            Some(hm) => hm,
            None => return "??:??".into(),
        }
    };
    format!("{h:02}:{m:02}")
}

/// Name, or the first 8 characters (UTF-16 units, like JS) of the sessionId. Both are cleaned
/// here: the snapshot cleans names but keeps the sessionId raw, because it is the selection key.
pub fn session_label(name: &str, session_id: &str) -> String {
    if name.is_empty() {
        clip_utf16(&clean_text(session_id), 8).to_string()
    } else {
        clean_text(name)
    }
}

pub fn state_label(state: LiveStatus, ended_at: Option<i64>) -> String {
    if ended_at.is_some() {
        "ended".into()
    } else if state == LiveStatus::Waiting {
        "needs you".into()
    } else {
        state.as_str().into()
    }
}

pub fn profile_header(profile: &str) -> String {
    format!(".{profile}")
}

pub fn role_label(role: Role, from_name: Option<&str>) -> String {
    match role {
        Role::Owner => "you".into(),
        Role::Peer => format!("peer:{}", from_name.unwrap_or("?")),
        Role::Assistant => "claude".into(),
    }
}

/// Hard-wraps text to `width` columns (by code point), on spaces where possible. Every output
/// line, after the first of each paragraph, gets `indent`. Like the TS loop, the spaces at the
/// start of a paragraph are dropped.
pub fn wrap(text: &str, width: usize, indent: &str) -> Vec<String> {
    let w = width.max(4) as i64;
    let indent_len = indent.chars().count() as i64;
    let mut out = Vec::new();
    for para in text.split('\n') {
        if para.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        let mut len = 0i64;
        let mut first = true;
        for word in para.split(' ') {
            let chars: Vec<char> = word.chars().collect();
            let mut rest: &[char] = &chars;
            let at_start = line.is_empty() || line == indent;
            let sep = i64::from(!at_start);
            if sep + rest.len() as i64 <= w - len {
                if sep == 1 {
                    line.push(' ');
                }
                line.push_str(word);
                len += sep + rest.len() as i64;
                continue;
            }
            if !at_start {
                out.push(std::mem::replace(&mut line, indent.to_string()));
                len = indent_len;
                first = false;
            }
            while !rest.is_empty() && rest.len() as i64 > w - len {
                let n = ((w - len).max(1) as usize).min(rest.len());
                line.extend(&rest[..n]);
                rest = &rest[n..];
                out.push(std::mem::replace(&mut line, indent.to_string()));
                len = indent_len;
                first = false;
            }
            line.extend(rest);
            len += rest.len() as i64;
        }
        if first || (!line.is_empty() && line != indent) {
            out.push(line);
        }
    }
    out
}
