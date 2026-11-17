//! Port of `test/platform/sysbin.test.ts` (the pure half; the source scans live in hub-platform's
//! tests, which see every crate). The env is never changed: `system32_in` takes the root.
use std::ffi::OsStr;
use std::path::PathBuf;

use hub_core::sysbin::{self, system32_in};

fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
}

#[test]
fn paths_are_built_from_system_root() {
    let root = Some(OsStr::new("X:\\Win"));
    assert_eq!(
        system32_in(root, "icacls.exe"),
        p("X:\\Win\\System32\\icacls.exe")
    );
    assert_eq!(
        system32_in(root, "reg.exe"),
        p("X:\\Win\\System32\\reg.exe")
    );
    assert_eq!(
        system32_in(root, "taskkill.exe"),
        p("X:\\Win\\System32\\taskkill.exe")
    );
    assert_eq!(
        system32_in(root, "whoami.exe"),
        p("X:\\Win\\System32\\whoami.exe")
    );
    assert_eq!(
        system32_in(root, "cmd.exe"),
        p("X:\\Win\\System32\\cmd.exe")
    );
    assert_eq!(
        system32_in(root, "WindowsPowerShell\\v1.0\\powershell.exe"),
        p("X:\\Win\\System32\\WindowsPowerShell\\v1.0\\powershell.exe")
    );
}

#[test]
fn system_root_unset_falls_back_to_c_windows() {
    assert_eq!(
        system32_in(None, "reg.exe"),
        p("C:\\Windows\\System32\\reg.exe")
    );
}

#[test]
fn an_empty_system_root_is_kept_like_ts() {
    // TS `process.env.SystemRoot ?? "C:\\Windows"` falls back only when the variable is unset.
    assert_eq!(
        system32_in(Some(OsStr::new("")), "reg.exe"),
        p("\\System32\\reg.exe")
    );
}

#[test]
fn each_tool_reads_system_root_from_the_env_at_call_time() {
    let root = std::env::var_os("SystemRoot");
    let r = root.as_deref();
    assert_eq!(sysbin::cmd(), system32_in(r, "cmd.exe"));
    assert_eq!(sysbin::icacls(), system32_in(r, "icacls.exe"));
    assert_eq!(
        sysbin::powershell(),
        system32_in(r, "WindowsPowerShell\\v1.0\\powershell.exe")
    );
    assert_eq!(sysbin::reg(), system32_in(r, "reg.exe"));
    assert_eq!(sysbin::taskkill(), system32_in(r, "taskkill.exe"));
    assert_eq!(sysbin::whoami(), system32_in(r, "whoami.exe"));
}
