//! Incremental transcript tail: reads new complete lines from a growing JSONL file starting at a
//! stored offset, never loading the whole file. Port of `src/ingest/tail.ts`.

use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

const NL: u8 = b'\n';
const HEAD_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TailState {
    pub path: String,
    pub offset: u64,
    pub size: u64,
    pub first_uuid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadNew {
    pub lines: Vec<String>,
    pub next: TailState,
    /// True after truncation or a replaced file: no lines, offset 0; call again to read from 0.
    pub reset: bool,
}

/// Reads bytes `[start, end)`, clamped to the end of the file (short reads possible).
fn read_range(file: &mut File, start: u64, end: u64) -> io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    file.take(end.saturating_sub(start)).read_to_end(&mut buf)?;
    Ok(buf)
}

/// JS `String.prototype.trim` whitespace: Unicode White_Space except U+0085, plus U+FEFF.
fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{FEFF}')
}

/// Decodes like `TextDecoder("utf-8")`: lossy, and a leading BOM is dropped.
fn decode(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
}

/// uuid of the first complete JSON line (within the first 64 KB) that has a string one.
fn head_uuid(file: &mut File, size: u64) -> io::Result<Option<String>> {
    let buf = read_range(file, 0, size.min(HEAD_BYTES))?;
    let mut start = 0;
    while start < buf.len() {
        let Some(rel) = buf[start..].iter().position(|&b| b == NL) else {
            return Ok(None);
        };
        let nl = start + rel;
        let text = decode(&buf[start..nl]);
        let text = js_trim(&text);
        start = nl + 1;
        if text.is_empty() {
            continue;
        }
        if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(text)
            && let Some(serde_json::Value::String(u)) = map.get("uuid")
        {
            return Ok(Some(u.clone()));
        }
    }
    Ok(None)
}

pub fn initial_state(path: &Path, max_first_bytes: u64) -> io::Result<TailState> {
    let size = std::fs::metadata(path)?.len();
    Ok(TailState {
        path: path.to_string_lossy().into_owned(),
        offset: size.saturating_sub(max_first_bytes),
        size,
        first_uuid: None,
    })
}

/// Reads complete new lines after `state.offset`, at most about `max_bytes` per call (more only to
/// finish a single longer line). Replaced-file detection is skipped when no line with a `uuid`
/// exists in the first 64 KB, or when the stored first uuid is `None`.
pub fn read_new(state: &TailState, max_bytes: u64) -> io::Result<ReadNew> {
    if max_bytes < 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "max_bytes must be >= 1",
        ));
    }
    let path = &state.path;
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let reset_state = || TailState {
        path: path.clone(),
        offset: 0,
        size,
        first_uuid: None,
    };

    if size < state.offset {
        return Ok(ReadNew {
            lines: vec![],
            next: reset_state(),
            reset: true,
        });
    }

    let head = head_uuid(&mut file, size)?;
    if state.offset > 0
        && let (Some(stored), Some(h)) = (&state.first_uuid, &head)
        && h != stored
    {
        return Ok(ReadNew {
            lines: vec![],
            next: reset_state(),
            reset: true,
        });
    }
    let first_uuid = head.or_else(|| state.first_uuid.clone());
    let keep = |offset: u64| TailState {
        path: path.clone(),
        offset,
        size,
        first_uuid: first_uuid.clone(),
    };

    let mut pos = state.offset;
    // Offsets we produce always follow a newline. Anything else (initial tail start) is aligned.
    if pos > 0 && pos < size {
        let prev = read_range(&mut file, pos - 1, pos)?;
        if prev.first() != Some(&NL) {
            let mut scan = pos;
            let mut found = None;
            while scan < size {
                let end = (scan + max_bytes).min(size);
                let chunk = read_range(&mut file, scan, end)?;
                if let Some(i) = chunk.iter().position(|&b| b == NL) {
                    found = Some(scan + i as u64 + 1);
                    break;
                }
                if chunk.is_empty() {
                    break;
                }
                scan += chunk.len() as u64;
            }
            match found {
                Some(f) => pos = f,
                // Offset size: the next call sees a non-newline byte before it and drops the rest.
                None => {
                    return Ok(ReadNew {
                        lines: vec![],
                        next: keep(size),
                        reset: false,
                    });
                }
            }
        }
    }

    let mut joined: Vec<u8> = Vec::new();
    let mut read_end = pos;
    let mut last_nl: Option<usize> = None;
    while read_end < size {
        let end = (read_end + max_bytes).min(size);
        let chunk = read_range(&mut file, read_end, end)?;
        if chunk.is_empty() {
            break;
        }
        let short = (chunk.len() as u64) < end - read_end;
        if let Some(i) = chunk.iter().rposition(|&b| b == NL) {
            last_nl = Some(joined.len() + i);
        }
        read_end += chunk.len() as u64;
        joined.extend_from_slice(&chunk);
        if last_nl.is_some() || short {
            break;
        }
    }

    let Some(last_nl) = last_nl else {
        return Ok(ReadNew {
            lines: vec![],
            next: keep(pos),
            reset: false,
        });
    };

    // Only bytes up to the last newline are decoded; the partial tail is never touched.
    let text = decode(&joined[..last_nl]);
    let lines = text
        .split('\n')
        .map(|raw| raw.strip_suffix('\r').unwrap_or(raw))
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    Ok(ReadNew {
        lines,
        next: keep(pos + last_nl as u64 + 1),
        reset: false,
    })
}
