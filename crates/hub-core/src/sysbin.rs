//! Absolute paths of the Windows tools the hub runs, so a same-named exe in the cwd or earlier on
//! PATH is never run. Built at call time from `SystemRoot`. Port of `src/platform/windows/sysbin.ts`;
//! the only place that builds a system-folder path.
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

/// `<root>\System32\<rel>`; `root` None means `C:\Windows` (TS `SystemRoot ?? "C:\\Windows"`, so an
/// empty value is kept). Pure, so tests never touch the env.
pub fn system32_in(root: Option<&OsStr>, rel: &str) -> PathBuf {
    let mut p = OsString::from(root.unwrap_or(OsStr::new("C:\\Windows")));
    p.push("\\System32\\");
    p.push(rel);
    PathBuf::from(p)
}

fn system32(rel: &str) -> PathBuf {
    system32_in(std::env::var_os("SystemRoot").as_deref(), rel)
}

pub fn cmd() -> PathBuf {
    system32("cmd.exe")
}

pub fn icacls() -> PathBuf {
    system32("icacls.exe")
}

pub fn powershell() -> PathBuf {
    system32("WindowsPowerShell\\v1.0\\powershell.exe")
}

pub fn reg() -> PathBuf {
    system32("reg.exe")
}

pub fn taskkill() -> PathBuf {
    system32("taskkill.exe")
}

pub fn whoami() -> PathBuf {
    system32("whoami.exe")
}
