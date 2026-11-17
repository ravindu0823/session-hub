//! Transcript discovery: `<profile>/projects/<one level>/*.jsonl`, the session-id check that guards
//! every path joined from an id, and the first cwd in a transcript's head. Port of
//! `src/daemon/transcripts.ts`.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::ingest::records::replace_lone_surrogates;

const CWD_HEAD_BYTES: u64 = 256 * 1024;
const JSONL: &str = ".jsonl";

/// cwd of the first record in the file's first 256 KB that has a string one, else `""`. An error
/// when the file cannot be opened or read (TS rejects).
pub fn first_cwd(path: &Path) -> io::Result<String> {
    let mut buf = Vec::new();
    File::open(path)?
        .take(CWD_HEAD_BYTES)
        .read_to_end(&mut buf)?;
    // Blob.text(): UTF-8, lossy, a leading BOM dropped.
    let bytes = buf.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(&buf);
    let text = String::from_utf8_lossy(bytes);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.len() > 1 {
        lines.pop(); // possibly cut mid-line
    }
    for line in lines {
        // JSON.parse accepts lone surrogate escapes; serde_json needs them replaced first.
        if let Ok(Value::Object(o)) = serde_json::from_str::<Value>(&replace_lone_surrogates(line))
            && let Some(Value::String(cwd)) = o.get("cwd")
        {
            return Ok(cwd.clone());
        }
    }
    Ok(String::new())
}

/// True for a name that must never be stat'ed (security rule: `*.key`). Such entries are skipped by
/// name before any file type check, which may stat.
fn never_stat(name: &[u8]) -> bool {
    name.ends_with(b".key")
}

/// Lexical normalisation like Node's `path.normalize`: separators become this OS's (`/` turns into
/// `\` on Windows), repeated separators and `.` segments go, and `..` drops the segment before it.
/// TS builds every transcript path with `path.join`, which normalises like this, and the path is
/// the key of the `tails` table both daemons share.
pub fn node_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                // `/..` is `/`; a relative path keeps its leading `..`.
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(".."),
            },
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// `<profile>/projects/<one level>/*.jsonl` files, as `path.join(profile, "projects", dir, file)`
/// (see [`node_normalize`]). Never recurses deeper, never follows a symlink, never opens a file. A
/// missing or unreadable `projects` folder lists nothing, and an unreadable project folder is
/// skipped, as in TS; the `Result` is never an error today.
pub fn list_transcripts(profile_dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let Ok(dirs) = fs::read_dir(node_normalize(profile_dir).join("projects")) else {
        return Ok(out);
    };
    for d in dirs.flatten() {
        if never_stat(d.file_name().as_encoded_bytes()) {
            continue;
        }
        // Not followed: a symlink is neither a dir nor a file here (Dirent semantics).
        if !d.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(files) = fs::read_dir(d.path()) else {
            continue; // unreadable folder: skip
        };
        for f in files.flatten() {
            if !f.file_name().as_encoded_bytes().ends_with(JSONL.as_bytes()) {
                continue;
            }
            if f.file_type().is_ok_and(|t| t.is_file()) {
                out.push(f.path());
            }
        }
    }
    Ok(out)
}

/// True for a uuid-shaped session id (`/^[0-9a-f-]{36}$/i`): the only ids ever joined into a
/// transcript path.
pub fn is_session_id(id: &str) -> bool {
    id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

/// stem -> path for every transcript of a profile (the first listed wins): one directory read per
/// project folder.
pub fn transcript_index(profile_dir: &Path) -> io::Result<HashMap<String, PathBuf>> {
    let mut index = HashMap::new();
    for path in list_transcripts(profile_dir)? {
        let Some(name) = path.file_name() else {
            continue;
        };
        let name = name.to_string_lossy();
        let stem = name.strip_suffix(JSONL).unwrap_or(&name).to_string();
        index.entry(stem).or_insert(path);
    }
    Ok(index)
}
