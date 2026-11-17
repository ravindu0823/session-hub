//! Ports of `test/platform/{acl-parse,exec,sysbin}.test.ts` (pure, every OS) and
//! `test/windows/platform.test.ts` (behind `SESSION_HUB_WINDOWS_SUITE=1` on Windows), plus the
//! native platform's process and privacy behaviour on the OS the tests run on.
use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hub_core::daemon::lock::{
    DaemonIdentity, LOCK_FILE, LockInfo, LockOptions, acquire_lock, format_lock, identify_daemon,
};
use hub_core::platform::{FocusResult, Platform};
use hub_platform::exec::{self, DEFAULT_TIMEOUT, ExecResult};
use hub_platform::windows::acl::{Identity, blocking_principals, parse_principals, parse_whoami};
use tempfile::TempDir;

const HELPER: &str = env!("CARGO_BIN_EXE_platform-helper");

fn tmp(prefix: &str) -> TempDir {
    tempfile::Builder::new().prefix(prefix).tempdir().unwrap()
}

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn helper_argv(args: &[&str]) -> Vec<OsString> {
    let mut v = vec![OsString::from(HELPER)];
    v.extend(os(args));
    v
}

/// Polls `cond` for up to `limit`.
fn eventually(limit: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let t0 = Instant::now();
    loop {
        if cond() {
            return true;
        }
        if t0.elapsed() >= limit {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

// ---------------------------------------------------------------- acl-parse.test.ts

fn me() -> Identity {
    Identity {
        name: "box\\alice".into(),
        sid: "s-1-5-21-1-2-3-1001".into(),
    }
}

const DIR: &str = "C:\\Users\\alice\\My Data";

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn parse_principals_strips_a_path_with_spaces_prefix_and_reads_continuation_lines() {
    let out = [
        &format!("{DIR} BOX\\alice:(OI)(CI)(F)"),
        "                  NT AUTHORITY\\SYSTEM:(OI)(CI)(F)",
        "",
        "Successfully processed 1 files; Failed processing 0 files",
    ]
    .join("\r\n");
    assert_eq!(
        parse_principals(&out, DIR),
        strings(&["BOX\\alice", "NT AUTHORITY\\SYSTEM"])
    );
}

#[test]
fn parse_principals_reads_inherited_lines() {
    let out = [
        &format!("{DIR} BUILTIN\\Administrators:(I)(OI)(CI)(F)"),
        "       BUILTIN\\Users:(I)(OI)(CI)(RX)",
        "",
    ]
    .join("\n");
    assert_eq!(
        parse_principals(&out, DIR),
        strings(&["BUILTIN\\Administrators", "BUILTIN\\Users"])
    );
}

#[test]
fn parse_principals_reads_unresolved_sids_and_creator_owner() {
    let out = [
        &format!("{DIR} S-1-5-21-9-9-9-500:(F)"),
        "       CREATOR OWNER:(OI)(CI)(IO)(F)",
        "",
    ]
    .join("\n");
    assert_eq!(
        parse_principals(&out, DIR),
        strings(&["S-1-5-21-9-9-9-500", "CREATOR OWNER"])
    );
}

#[test]
fn parse_principals_ignores_the_trailing_summary() {
    assert!(parse_principals("\nSuccessfully processed 1 files\n", DIR).is_empty());
}

#[test]
fn parse_principals_matches_the_dir_prefix_case_insensitively_and_needs_a_name_before_the_colon() {
    let out = format!(
        "{} BOX\\alice:(F)\n   :(F)\n  no grant here\n",
        DIR.to_uppercase()
    );
    assert_eq!(parse_principals(&out, DIR), strings(&["BOX\\alice"]));
}

#[test]
fn blocking_principals_allows_the_current_user_by_name_or_sid_and_system_by_name_or_sid() {
    let p = strings(&[
        "BOX\\Alice",
        "S-1-5-21-1-2-3-1001",
        "NT AUTHORITY\\SYSTEM",
        "S-1-5-18",
    ]);
    assert!(blocking_principals(&p, &me()).is_empty());
}

#[test]
fn blocking_principals_administrators_users_everyone_creator_owner_and_unknown_sids_block() {
    let p = strings(&[
        "BUILTIN\\Administrators",
        "BUILTIN\\Users",
        "Everyone",
        "CREATOR OWNER",
        "S-1-5-21-9-9-9-500",
    ]);
    assert_eq!(blocking_principals(&p, &me()).len(), 5);
}

#[test]
fn blocking_principals_a_localized_system_name_blocks_only_the_sid_form_is_trusted() {
    assert_eq!(
        blocking_principals(&strings(&["NT-AUTORITÄT\\SYSTEM"]), &me()),
        strings(&["NT-AUTORITÄT\\SYSTEM"])
    );
    assert!(blocking_principals(&strings(&["S-1-5-18"]), &me()).is_empty());
}

#[test]
fn parse_whoami_reads_name_and_sid_lower_cased() {
    assert_eq!(
        parse_whoami("\r\n\"BOX\\Alice\",\"S-1-5-21-1-2-3-1001\"\r\n"),
        Some(me())
    );
    assert_eq!(parse_whoami(""), None);
    assert_eq!(parse_whoami("\"BOX\\alice\",\"X-1-5\""), None);
    assert_eq!(parse_whoami("\"\",\"S-1-5-18\""), None);
}

// ---------------------------------------------------------------- exec.test.ts

#[test]
fn run_default_timeout_is_10_s() {
    assert_eq!(DEFAULT_TIMEOUT, Duration::from_secs(10));
}

#[test]
fn run_a_given_timeout_stops_a_slow_child() {
    let t0 = Instant::now();
    let r = exec::run(
        &helper_argv(&["sleep", "20"]),
        &[],
        Duration::from_millis(300),
    )
    .unwrap();
    assert_ne!(r.code, 0);
    assert!(t0.elapsed() < Duration::from_secs(5));
}

#[test]
fn run_a_child_that_finishes_in_time_returns_its_output_and_exit_code() {
    let r = exec::run(
        &helper_argv(&["echo-env-exit", "SH_X", "3"]),
        &[("SH_X", "ok")],
        Duration::from_secs(10),
    )
    .unwrap();
    assert_eq!(
        r,
        ExecResult {
            code: 3,
            stdout: "ok".into()
        }
    );
}

#[test]
fn run_checked_names_the_tool_and_exit_code_only() {
    let e = exec::run_checked(&helper_argv(&["echo-env-exit", "SH_SECRET", "4"])).unwrap_err();
    assert_eq!(e.to_string(), "platform-helper failed with exit code 4");
    let ok = exec::run_checked(&helper_argv(&["echo-env-exit", "NOPE_UNSET_VAR", "0"])).unwrap();
    assert_eq!(ok.code, 0);
}

#[test]
fn run_a_missing_program_is_an_error() {
    let t = tmp("hub-exec-");
    let missing = t.path().join("no-such-tool");
    assert!(exec::run(&[missing.into_os_string()], &[], DEFAULT_TIMEOUT).is_err());
}

#[test]
fn tool_name_is_the_base_name_without_exe() {
    assert_eq!(
        exec::tool_name(Path::new("C:\\Windows\\System32\\icacls.exe")),
        "icacls"
    );
    assert_eq!(exec::tool_name(Path::new("/usr/bin/whoami")), "whoami");
    assert_eq!(exec::tool_name(Path::new("X.EXE")), "X");
    assert_eq!(exec::tool_name(Path::new("")), "");
}

// ---------------------------------------------------------------- sysbin.test.ts (source scans)

fn crate_sources() -> Vec<(PathBuf, String)> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut out = Vec::new();
    let mut stack: Vec<PathBuf> = std::fs::read_dir(crates)
        .unwrap()
        .map(|e| e.unwrap().path().join("src"))
        .filter(|p| p.is_dir())
        .collect();
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                let text = std::fs::read_to_string(&p).unwrap();
                out.push((p, text));
            }
        }
    }
    assert!(out.len() > 10, "found the crates' sources");
    out
}

#[test]
fn no_source_file_runs_a_windows_system_tool_by_bare_name() {
    let bare = regex::Regex::new(
        r#"(?i)Command::new\(\s*"(?:icacls|reg|powershell|taskkill|whoami|cmd)(?:\.exe)?""#,
    )
    .unwrap();
    let offenders: Vec<_> = crate_sources()
        .into_iter()
        .filter(|(_, t)| bare.is_match(t))
        .map(|(p, _)| p)
        .collect();
    assert!(offenders.is_empty(), "{offenders:?}");
}

#[test]
fn only_sysbin_builds_a_system_folder_path() {
    let sys = regex::Regex::new(r"(?i)System32").unwrap();
    let offenders: Vec<_> = crate_sources()
        .into_iter()
        .filter(|(p, t)| !p.ends_with("sysbin.rs") && sys.is_match(t))
        .map(|(p, _)| p)
        .collect();
    assert!(offenders.is_empty(), "{offenders:?}");
}

// ---------------------------------------------------------------- native platform, every OS

#[test]
fn own_process_info() {
    let p = hub_platform::native();
    let me = std::process::id();
    assert!(p.process_alive(me));
    let started = p.process_start_time(me).expect("own start time");
    assert!(
        (now_ms() - started).abs() < 60_000,
        "started {started}, now {}",
        now_ms()
    );
    let image = p.process_image_path(me).expect("own image path");
    let exe = std::env::current_exe().unwrap();
    assert_eq!(image.file_name(), exe.file_name());
}

#[test]
fn a_reaped_child_is_not_alive_and_has_no_info() {
    let p = hub_platform::native();
    let mut child = Command::new(HELPER)
        .args(["echo-env-exit", "NOPE_UNSET_VAR", "0"])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    child.wait().unwrap();
    // On Windows the Child holds a process handle, which keeps the process object queryable.
    drop(child);
    assert!(!p.process_alive(pid));
    assert_eq!(p.process_start_time(pid), None);
    assert_eq!(p.process_image_path(pid), None);
}

#[test]
fn a_live_child_has_info() {
    let p = hub_platform::native();
    let mut child = Command::new(HELPER).args(["sleep", "30"]).spawn().unwrap();
    let pid = child.id();
    assert!(p.process_alive(pid));
    let started = p.process_start_time(pid).expect("start time");
    assert!((now_ms() - started).abs() < 60_000);
    let image = p.process_image_path(pid).expect("image");
    assert_eq!(image.file_name(), Path::new(HELPER).file_name());
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn pid_zero_is_never_alive_and_has_no_info() {
    let p = hub_platform::native();
    assert!(!p.process_alive(0));
    assert_eq!(p.process_start_time(0), None);
    assert_eq!(p.process_image_path(0), None);
}

// ---------------------------------------------------------------- daemon lock, real queries

fn real_identify(l: &LockInfo) -> DaemonIdentity {
    identify_daemon(l, hub_platform::native().as_ref(), false)
}

/// Port of lock.test.ts "real check" (Windows-only there; every OS here).
#[test]
fn lock_real_check_a_live_process_with_the_recorded_start_time_is_ours_another_start_is_stale() {
    let p = hub_platform::native();
    let d = tmp("hub-lock-");
    let file = d.path().join(LOCK_FILE);
    let started = p.process_start_time(std::process::id()).expect("own start");
    // The start time is stable from one read to the next.
    assert_eq!(p.process_start_time(std::process::id()), Some(started));
    let at = |t: i64| {
        format_lock(&LockInfo {
            pid: std::process::id(),
            started_at: Some(t),
        })
    };
    std::fs::write(&file, at(started)).unwrap();
    let opts = || LockOptions {
        identify: &real_identify,
        started_at: None,
    };
    assert!(acquire_lock(d.path(), opts()).unwrap().is_none());
    // The same pid recorded with another start time: the pid was reused.
    std::fs::write(&file, at(started - 60_000)).unwrap();
    let lock = acquire_lock(d.path(), opts()).unwrap();
    assert!(lock.is_some());
}

#[test]
fn lock_real_check_a_legacy_lock_naming_another_program_is_stale() {
    let d = tmp("hub-lock-");
    let mut child = Command::new(HELPER).args(["sleep", "30"]).spawn().unwrap();
    std::fs::write(d.path().join(LOCK_FILE), child.id().to_string()).unwrap();
    let lock = acquire_lock(
        d.path(),
        LockOptions {
            identify: &real_identify,
            started_at: None,
        },
    )
    .unwrap();
    assert!(lock.is_some());
    child.kill().unwrap();
    child.wait().unwrap();
}

/// Spawns `helper tree` hidden and returns the child and its grandchild's pid.
fn spawn_tree(p: &dyn Platform) -> (Child, u32) {
    spawn_tree_mode(p, "tree")
}

fn spawn_tree_mode(p: &dyn Platform, mode: &str) -> (Child, u32) {
    let mut cmd = Command::new(HELPER);
    cmd.arg(mode)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    p.spawn_hidden(&mut cmd);
    let mut child = cmd.spawn().unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let gc: u32 = line.trim().parse().expect("grandchild pid");
    (child, gc)
}

#[test]
fn kill_tree_kills_children() {
    let p = hub_platform::native();
    let (mut child, gc) = spawn_tree(p.as_ref());
    assert!(p.process_alive(gc));
    p.kill_tree(child.id()).unwrap();
    let t0 = Instant::now();
    let status = child.wait().unwrap();
    assert!(!status.success());
    assert!(t0.elapsed() < Duration::from_secs(5));
    assert!(
        eventually(Duration::from_secs(5), || !p.process_alive(gc)),
        "grandchild {gc} still alive"
    );
}

#[test]
fn kill_tree_of_a_missing_pid_is_an_error() {
    let p = hub_platform::native();
    let mut child = Command::new(HELPER)
        .args(["echo-env-exit", "NOPE_UNSET_VAR", "0"])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    child.wait().unwrap();
    drop(child);
    assert!(p.kill_tree(pid).is_err());
}

/// Ruling R11: pids whose tree kill would reach other processes are refused before any signal or
/// `taskkill`.
#[test]
fn kill_tree_refuses_dangerous_pids() {
    let p = hub_platform::native();
    let refused = |pid: u32| {
        p.kill_tree(pid)
            .is_err_and(|e| e.kind() == std::io::ErrorKind::InvalidInput)
    };
    assert!(refused(0));
    assert!(refused(std::process::id()));
    if cfg!(windows) {
        assert!(refused(4));
    } else {
        assert!(refused(1));
    }
}

// ---------------------------------------------------------------- Unix

#[cfg(unix)]
mod unix {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn unix_private_dir_mode() {
        let p = hub_platform::native();
        let t = tmp("hub-priv-");
        let d = t.path().join("a").join("data");
        assert!(!p.is_private_dir(&d).unwrap(), "missing is not private");
        p.ensure_private_dir(&d).unwrap();
        assert_eq!(mode(&d), 0o700);
        assert!(p.is_private_dir(&d).unwrap());
        // Only the dir itself is made private; a created parent keeps the default mode.
        let reference = t.path().join("reference");
        std::fs::create_dir(&reference).unwrap();
        assert_eq!(mode(&t.path().join("a")), mode(&reference));
        // Ensuring again is fine.
        p.ensure_private_dir(&d).unwrap();

        let open = t.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!p.is_private_dir(&open).unwrap());
        p.ensure_private_dir(&open).unwrap();
        assert_eq!(mode(&open), 0o700);
        assert!(p.is_private_dir(&open).unwrap());
    }

    #[test]
    fn a_file_is_not_a_private_dir_and_cannot_be_made_one() {
        let p = hub_platform::native();
        let t = tmp("hub-priv-");
        let f = t.path().join("file");
        std::fs::write(&f, "x").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!p.is_private_dir(&f).unwrap());
        assert!(p.ensure_private_dir(&f).is_err());
    }

    #[test]
    fn a_dir_owned_by_someone_else_is_not_private_and_is_not_changed() {
        let p = hub_platform::native();
        // `/` belongs to root; the tests do not run as root.
        if rustix::process::geteuid().is_root() {
            return;
        }
        assert!(!p.is_private_dir(Path::new("/")).unwrap());
        assert!(p.ensure_private_dir(Path::new("/")).is_err());
    }

    #[test]
    fn spawn_hidden_puts_the_child_in_its_own_process_group() {
        let p = hub_platform::native();
        let mut cmd = Command::new(HELPER);
        cmd.args(["sleep", "30"]);
        p.spawn_hidden(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        let pid = rustix::process::Pid::from_child(&child);
        assert_eq!(rustix::process::getpgid(Some(pid)).unwrap(), pid);
        p.kill_tree(child.id()).unwrap();
        assert!(!child.wait().unwrap().success());
    }

    /// A child that shares our process group (not spawned hidden) is killed alone: the group kill
    /// must never reach the test process.
    #[test]
    fn kill_tree_of_a_child_without_its_own_group_kills_only_that_pid() {
        let p = hub_platform::native();
        let mut child = Command::new(HELPER).args(["sleep", "30"]).spawn().unwrap();
        p.kill_tree(child.id()).unwrap();
        assert!(!child.wait().unwrap().success());
        assert!(p.process_alive(std::process::id()));
    }

    /// The group leader has exited but is not reaped (a zombie; macOS `getpgid` then gives ESRCH)
    /// while a grandchild still runs in its group: the group kill must still reach the grandchild.
    #[test]
    fn kill_tree_reaches_the_group_of_an_unreaped_exited_leader() {
        use std::io::Read;
        let p = hub_platform::native();
        let mut cmd = Command::new(HELPER);
        cmd.arg("tree-exit")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        p.spawn_hidden(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        let mut text = String::new();
        // EOF: the child closed stdout by exiting (the grandchild's stdout is null).
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        let gc: u32 = text.trim().parse().expect("grandchild pid");
        std::thread::sleep(Duration::from_millis(200));
        assert!(p.process_alive(gc));
        let _ = p.kill_tree(child.id());
        let gone = eventually(Duration::from_secs(5), || !p.process_alive(gc));
        if !gone {
            let _ = rustix::process::kill_process(
                rustix::process::Pid::from_raw(gc as i32).unwrap(),
                rustix::process::Signal::KILL,
            );
        }
        child.wait().unwrap();
        assert!(gone, "grandchild {gc} survived the tree kill");
    }

    #[test]
    fn kill_tree_never_signals_the_own_process_group() {
        let p = hub_platform::native();
        let own_group = rustix::process::getpgrp().as_raw_nonzero().get() as u32;
        assert!(p.kill_tree(own_group).is_err());
        assert!(p.process_alive(std::process::id()));
    }

    #[test]
    fn kill_tree_refuses_pid_zero_and_the_own_pid() {
        let p = hub_platform::native();
        assert!(p.kill_tree(0).is_err());
        assert!(p.kill_tree(std::process::id()).is_err());
        assert!(p.process_alive(std::process::id()));
    }

    #[test]
    fn focus_is_unsupported() {
        let p = hub_platform::native();
        assert_eq!(
            p.focus_session_window(std::process::id()),
            FocusResult::Unsupported
        );
    }
}

// ---------------------------------------------------------------- windows/platform.test.ts

#[cfg(windows)]
mod windows_suite {
    use std::path::Path;

    use super::*;

    /// Focuses windows and changes ACLs: only with `SESSION_HUB_WINDOWS_SUITE=1`, like TS's
    /// `bun run test:windows`.
    fn suite() -> bool {
        let on = std::env::var("SESSION_HUB_WINDOWS_SUITE").as_deref() == Ok("1");
        if !on {
            eprintln!("skipped: set SESSION_HUB_WINDOWS_SUITE=1");
        }
        on
    }

    fn icacls(args: &[&OsString]) -> i32 {
        let mut cmd = Command::new(hub_core::sysbin::icacls());
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        hub_platform::native().spawn_hidden(&mut cmd);
        cmd.status().unwrap().code().unwrap_or(-1)
    }

    fn grant_users_read(d: &Path) -> i32 {
        icacls(&[
            &d.as_os_str().to_owned(),
            &OsString::from("/grant"),
            &OsString::from("*S-1-5-32-545:(OI)(CI)R"),
        ])
    }

    #[test]
    fn ensure_private_dir_makes_the_dir_private() {
        if !suite() {
            return;
        }
        let p = hub_platform::native();
        let t = tmp("sh-acl-");
        let d = t.path().join("data");
        p.ensure_private_dir(&d).unwrap();
        assert!(p.is_private_dir(&d).unwrap());
    }

    #[test]
    fn an_extra_users_grant_is_detected() {
        if !suite() {
            return;
        }
        let p = hub_platform::native();
        let t = tmp("sh-acl-");
        let d = t.path().join("d");
        p.ensure_private_dir(&d).unwrap();
        assert_eq!(grant_users_read(&d), 0);
        assert!(!p.is_private_dir(&d).unwrap());
    }

    #[test]
    fn an_explicit_grant_to_another_principal_is_removed() {
        if !suite() {
            return;
        }
        // An elevated admin's new folders can carry an explicit Administrators entry, which
        // /inheritance:r does not remove; Users stands in for it here.
        let p = hub_platform::native();
        let t = tmp("sh-acl-");
        let d = t.path().join("explicit");
        std::fs::create_dir(&d).unwrap();
        assert_eq!(grant_users_read(&d), 0);
        p.ensure_private_dir(&d).unwrap();
        assert!(p.is_private_dir(&d).unwrap());
    }

    #[test]
    fn a_default_inherited_temp_dir_is_not_private() {
        if !suite() {
            return;
        }
        let p = hub_platform::native();
        let t = tmp("sh-acl-");
        assert!(!p.is_private_dir(t.path()).unwrap());
    }

    #[test]
    fn a_missing_dir_is_not_private() {
        if !suite() {
            return;
        }
        let p = hub_platform::native();
        let t = tmp("sh-acl-");
        assert!(!p.is_private_dir(&t.path().join("nope")).unwrap());
    }

    #[test]
    fn a_path_with_spaces_works() {
        if !suite() {
            return;
        }
        let p = hub_platform::native();
        let t = tmp("sh-acl-");
        let d = t.path().join("a b c");
        std::fs::create_dir(&d).unwrap();
        p.ensure_private_dir(&d).unwrap();
        assert!(p.is_private_dir(&d).unwrap());
    }

    #[test]
    fn focus_own_pid_returns_a_well_formed_result() {
        if !suite() {
            return;
        }
        let r = hub_platform::native().focus_session_window(std::process::id());
        let reason = match &r {
            FocusResult::Focused { .. } => None,
            FocusResult::NotFound { reason } => Some(reason.as_str()),
            FocusResult::Unsupported => panic!("Windows supports focus"),
        };
        // Either outcome proves the parent walk found a window.
        if std::env::var("SESSION_HUB_HEADLESS").as_deref() != Ok("1") {
            assert!(reason.is_none() || reason == Some("focus-denied"), "{r:?}");
        } else if let Some(reason) = reason {
            assert!(["no-window", "focus-denied"].contains(&reason), "{r:?}");
        }
    }

    #[test]
    fn focus_an_invalid_pid_is_an_error() {
        if !suite() {
            return;
        }
        assert_eq!(
            hub_platform::native().focus_session_window(0),
            FocusResult::NotFound {
                reason: "error".into()
            }
        );
    }
}
