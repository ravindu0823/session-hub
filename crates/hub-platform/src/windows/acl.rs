//! Data-dir privacy on Windows through `icacls` and `whoami` (absolute paths from
//! `hub_core::sysbin`). Port of `src/platform/windows/acl.ts`; the parsers are portable.
use hub_core::text::{is_js_space, utf16_len};

const SYSTEM_NAME: &str = "nt authority\\system";
const SYSTEM_SID: &str = "s-1-5-18";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// `DOMAIN\user`, lower-case.
    pub name: String,
    /// `S-1-5-21-...`, lower-case.
    pub sid: String,
}

fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

/// `whoami /user /fo csv /nh` output: `"DOMAIN\user","S-..."`. None when it does not have that shape.
pub fn parse_whoami(stdout: &str) -> Option<Identity> {
    let re = regex::Regex::new(r#"^"([^"]+)","(S-[0-9-]+)""#).expect("valid regex");
    let m = re.captures(js_trim(stdout))?;
    Some(Identity {
        name: m[1].to_lowercase(),
        sid: m[2].to_lowercase(),
    })
}

/// Byte index in `s` after its first `units` UTF-16 units (JS `s.slice(units)`), rounded up to a
/// char boundary.
fn after_utf16_units(s: &str, units: usize) -> usize {
    let mut seen = 0;
    for (i, c) in s.char_indices() {
        if seen >= units {
            return i;
        }
        seen += c.len_utf16();
    }
    s.len()
}

/// Principals named in `icacls <dir>` output (ACE lines up to the first blank line).
pub fn parse_principals(output: &str, dir: &str) -> Vec<String> {
    let mut out = Vec::new();
    let dir_lower = dir.to_lowercase();
    for raw in hub_core::text::split_js_lines(output) {
        let mut line = js_trim(raw);
        if line.is_empty() {
            break;
        }
        if line.to_lowercase().starts_with(&dir_lower) {
            line = js_trim(&line[after_utf16_units(line, utf16_len(dir))..]);
        }
        // JS /^(.+?):\(/: the first ":(" after at least one character.
        let Some(first) = line.chars().next() else {
            continue;
        };
        if let Some(i) = line[first.len_utf8()..].find(":(") {
            out.push(js_trim(&line[..first.len_utf8() + i]).to_string());
        }
    }
    out
}

/// Principals that are neither the current user (name or SID) nor SYSTEM (by English name or SID).
pub fn blocking_principals(principals: &[String], me: &Identity) -> Vec<String> {
    principals
        .iter()
        .filter(|p| {
            let l = p.to_lowercase();
            l != me.name && l != me.sid && l != SYSTEM_NAME && l != SYSTEM_SID
        })
        .cloned()
        .collect()
}

#[cfg(windows)]
pub(crate) use real::{ensure_private_dir, is_private_dir};

#[cfg(windows)]
mod real {
    use std::ffi::OsString;
    use std::io;
    use std::path::Path;
    use std::sync::Mutex;

    use hub_core::sysbin;

    use super::{Identity, blocking_principals, parse_principals, parse_whoami};
    use crate::exec::{DEFAULT_TIMEOUT, run, run_checked};

    /// Resolved once; a failure is not cached, so the next call tries again (like TS).
    static IDENTITY: Mutex<Option<Identity>> = Mutex::new(None);

    fn current_identity() -> io::Result<Identity> {
        let mut cached = IDENTITY.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(id) = cached.as_ref() {
            return Ok(id.clone());
        }
        let r = run_checked(&[
            sysbin::whoami().into_os_string(),
            "/user".into(),
            "/fo".into(),
            "csv".into(),
            "/nh".into(),
        ])?;
        let id =
            parse_whoami(&r.stdout).ok_or_else(|| io::Error::other("whoami returned no SID"))?;
        *cached = Some(id.clone());
        Ok(id)
    }

    fn icacls(dir: &Path, rest: &[&str]) -> Vec<OsString> {
        let mut argv = vec![
            sysbin::icacls().into_os_string(),
            dir.as_os_str().to_owned(),
        ];
        argv.extend(rest.iter().map(OsString::from));
        argv
    }

    /// None: directory missing or unreadable. Empty: private. Otherwise the blocking principals.
    fn inspect(dir: &Path) -> io::Result<Option<Vec<String>>> {
        if !std::fs::metadata(dir).is_ok_and(|m| m.is_dir()) {
            return Ok(None);
        }
        let r = run(&icacls(dir, &[]), &[], DEFAULT_TIMEOUT)?;
        if r.code != 0 {
            return Ok(None);
        }
        let principals = parse_principals(&r.stdout, &dir.to_string_lossy());
        if principals.is_empty() {
            return Ok(None);
        }
        Ok(Some(blocking_principals(&principals, &current_identity()?)))
    }

    pub fn is_private_dir(dir: &Path) -> io::Result<bool> {
        Ok(inspect(dir)?.is_some_and(|b| b.is_empty()))
    }

    pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let me = current_identity()?;
        let grant_me = format!("*{}:(OI)(CI)F", me.sid.to_uppercase());
        run_checked(&icacls(
            dir,
            &[
                "/inheritance:r",
                "/grant:r",
                &grant_me,
                "*S-1-5-18:(OI)(CI)F",
            ],
        ))?;
        // /inheritance:r drops inherited entries and /grant:r replaces only the two grants above, so
        // an explicit entry for anyone else survives (an elevated admin's new folders can carry one
        // for BUILTIN\Administrators). Remove each remaining principal by the name icacls printed,
        // then verify.
        let mut blockers = inspect(dir)?;
        if let Some(b) = blockers.as_ref().filter(|b| !b.is_empty()) {
            for principal in b {
                run_checked(&icacls(dir, &["/remove", principal]))?;
            }
            blockers = inspect(dir)?;
        }
        match blockers {
            None => Err(io::Error::other(
                "directory is not private: icacls could not verify it",
            )),
            Some(b) if !b.is_empty() => Err(io::Error::other(format!(
                "directory is not private; blocking principals: {}",
                b.join(", ")
            ))),
            Some(_) => Ok(()),
        }
    }
}
