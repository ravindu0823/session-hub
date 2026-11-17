//! Port of `test/daemon/lock.test.ts`. The Windows-only "real check" tests (a live process's start
//! time and image) need the real platform and live in `crates/hub-platform/tests/platform.rs`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use hub_core::daemon::lock::{
    DaemonIdentity, LOCK_FILE, LockInfo, LockOptions, ProcessProbe, START_TIME_SLACK_MS,
    acquire_lock, daemon_identity, format_lock, identify_daemon, is_killable_image, parse_lock,
};
use hub_core::platform::{FakePlatform, FocusResult, Platform};
use tempfile::TempDir;

use DaemonIdentity::{Foreign, Ours, Unknown};

const T0: i64 = 1_763_000_000_000;

fn tmp() -> TempDir {
    tempfile::Builder::new()
        .prefix("hub-lock-")
        .tempdir()
        .unwrap()
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
}

fn me() -> u32 {
    std::process::id()
}

/// TS's default identity: a live pid is held ("unknown"), a dead one is stale.
fn alive_or_foreign(l: &LockInfo) -> DaemonIdentity {
    if FakePlatform.process_alive(l.pid) {
        Unknown
    } else {
        Foreign
    }
}

fn opts(
    identify: &dyn Fn(&LockInfo) -> DaemonIdentity,
    started_at: Option<i64>,
) -> LockOptions<'_> {
    LockOptions {
        identify,
        started_at,
    }
}

fn dead_pid() -> u32 {
    let exe = env!("CARGO_BIN_EXE_fake-claude");
    let mut c = Command::new(exe)
        .env("FAKE_CLAUDE_MODE", "garbage")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    c.wait().unwrap();
    c.id()
}

// ---------------------------------------------------------------- format

#[test]
fn lock_format_pid_and_start_time_and_the_legacy_pid_only_form_garbage_is_none() {
    assert_eq!(
        parse_lock("4242 1763000000000"),
        Some(LockInfo {
            pid: 4242,
            started_at: Some(T0)
        })
    );
    assert_eq!(
        parse_lock(" 4242\n"),
        Some(LockInfo {
            pid: 4242,
            started_at: None
        })
    );
    for bad in ["", "x", "0", "-1", "4242 x", "4242  1", "4242 1 2"] {
        assert_eq!(parse_lock(bad), None, "{bad:?}");
    }
    assert_eq!(
        format_lock(&LockInfo {
            pid: 4242,
            started_at: Some(T0)
        }),
        "4242 1763000000000"
    );
    assert_eq!(
        format_lock(&LockInfo {
            pid: 4242,
            started_at: None
        }),
        "4242"
    );
}

#[test]
fn lock_reads_ts_lock() {
    let l = parse_lock("1234 1731740000000").unwrap();
    assert_eq!(
        l,
        LockInfo {
            pid: 1234,
            started_at: Some(1_731_740_000_000)
        }
    );
    assert_eq!(
        parse_lock("1234"),
        Some(LockInfo {
            pid: 1234,
            started_at: None
        })
    );
    assert_eq!(parse_lock("x"), None);
    // Byte-identical to TS formatLock: no trailing newline.
    assert_eq!(format_lock(&l), "1234 1731740000000");
    assert_eq!(parse_lock(&format_lock(&l)), Some(l));
}

#[test]
fn parse_lock_follows_js_number_rules() {
    // Leading zeros are a number; JS trim also strips U+FEFF and U+3000.
    assert_eq!(
        parse_lock("\u{FEFF}0042 007\u{3000}"),
        Some(LockInfo {
            pid: 42,
            started_at: Some(7)
        })
    );
    // Above Number.MAX_SAFE_INTEGER is refused, also for the start time.
    assert_eq!(parse_lock("9007199254740992"), None);
    assert_eq!(parse_lock("1 9007199254740992"), None);
    assert_eq!(
        parse_lock("1 9007199254740991"),
        Some(LockInfo {
            pid: 1,
            started_at: Some(9_007_199_254_740_991)
        })
    );
    // A pid no process can have (beyond u32) is refused.
    assert_eq!(parse_lock("4294967296"), None);
    // ASCII digits only; one space between; no inner newline.
    for bad in ["٤٢", "４２", "42\t1", "42\n1", "+42", "42 -1"] {
        assert_eq!(parse_lock(bad), None, "{bad:?}");
    }
}

// ---------------------------------------------------------------- identity

fn at(started_at: Option<i64>, image: Option<&str>) -> ProcessProbe {
    ProcessProbe {
        alive: true,
        started_at,
        image: image.map(PathBuf::from),
    }
}

fn dead() -> ProcessProbe {
    ProcessProbe {
        alive: false,
        started_at: None,
        image: None,
    }
}

#[test]
fn with_a_recorded_start_time_same_start_within_2_s_is_ours_whatever_the_image() {
    let with_start = LockInfo {
        pid: 7,
        started_at: Some(T0),
    };
    assert_eq!(START_TIME_SLACK_MS, 2_000);
    assert_eq!(
        daemon_identity(&with_start, &at(Some(T0), None), false),
        Ours
    );
    assert_eq!(
        daemon_identity(
            &with_start,
            &at(Some(T0 + 2_000), Some("C:\\bun\\bun.exe")),
            false
        ),
        Ours
    );
    assert_eq!(
        daemon_identity(&with_start, &at(Some(T0 - 2_000), None), false),
        Ours
    );
    // A pid reused by another process (any image, bun.exe included) started later: stale.
    assert_eq!(
        daemon_identity(
            &with_start,
            &at(Some(T0 + 60_000), Some("C:\\bun\\bun.exe")),
            false
        ),
        Foreign
    );
    assert_eq!(
        daemon_identity(&with_start, &at(Some(T0 + 2_001), None), false),
        Foreign
    );
    assert_eq!(
        daemon_identity(&with_start, &at(Some(T0 - 2_001), None), false),
        Foreign
    );
    // Alive but its start time cannot be read (elevated, protected): held, never killed.
    assert_eq!(
        daemon_identity(&with_start, &at(None, None), false),
        Unknown
    );
    assert_eq!(daemon_identity(&with_start, &dead(), false), Foreign);
}

#[test]
fn a_legacy_pid_only_lock_falls_back_to_the_image_rule() {
    let legacy = LockInfo {
        pid: 7,
        started_at: None,
    };
    let img = |i: Option<&str>| at(None, i);
    let id = |i: Option<&str>, dev: bool| daemon_identity(&legacy, &img(i), dev);
    assert_eq!(id(Some("C:\\L\\SessionHub\\bin\\hubd.exe"), false), Ours);
    assert_eq!(id(Some("D:\\dist\\HUB.EXE"), false), Ours);
    assert_eq!(id(Some("D:\\dist\\hubw.exe"), false), Ours);
    assert_eq!(id(Some("C:\\bun\\bun.exe"), true), Ours);
    assert_eq!(id(Some("C:\\bun\\bun.exe"), false), Unknown);
    assert_eq!(id(None, false), Unknown);
    assert_eq!(
        id(Some("C:\\Windows\\System32\\notepad.exe"), true),
        Foreign
    );
    assert_eq!(id(Some("C:\\x\\hubd.exe.bak"), false), Foreign);
    assert_eq!(daemon_identity(&legacy, &dead(), false), Foreign);
    // The Rust daemon on Unix is `hub` (no extension).
    let unix_hub = id(Some("/usr/local/bin/hub"), false);
    assert_eq!(unix_hub, if cfg!(unix) { Ours } else { Foreign });
}

/// Records every query; `alive` decides liveness, the start time is T0 and the image hubd.exe.
struct Queries {
    alive: bool,
    asked: Mutex<Vec<String>>,
}

impl Platform for Queries {
    fn ensure_private_dir(&self, _: &Path) -> std::io::Result<()> {
        Ok(())
    }
    fn is_private_dir(&self, _: &Path) -> std::io::Result<bool> {
        Ok(true)
    }
    fn process_alive(&self, _: u32) -> bool {
        self.alive
    }
    fn process_start_time(&self, pid: u32) -> Option<i64> {
        self.asked.lock().unwrap().push(format!("start {pid}"));
        Some(T0)
    }
    fn process_image_path(&self, pid: u32) -> Option<PathBuf> {
        self.asked.lock().unwrap().push(format!("image {pid}"));
        Some(PathBuf::from("C:\\b\\hubd.exe"))
    }
    fn spawn_hidden(&self, _: &mut Command) {}
    fn kill_tree(&self, _: u32) -> std::io::Result<()> {
        unreachable!("never kills")
    }
    fn focus_session_window(&self, _: u32) -> FocusResult {
        FocusResult::Unsupported
    }
}

#[test]
fn identify_daemon_queries_a_live_pid_only_and_only_what_the_lock_format_needs() {
    let dead = Queries {
        alive: false,
        asked: Mutex::new(vec![]),
    };
    let live = Queries {
        alive: true,
        asked: Mutex::new(vec![]),
    };
    let l = |pid, started_at| LockInfo { pid, started_at };
    assert_eq!(identify_daemon(&l(5, Some(T0)), &dead, false), Foreign);
    assert_eq!(identify_daemon(&l(6, Some(T0)), &live, false), Ours);
    assert_eq!(identify_daemon(&l(8, None), &live, false), Ours);
    assert!(dead.asked.lock().unwrap().is_empty());
    assert_eq!(*live.asked.lock().unwrap(), ["start 6", "image 8"]);
}

#[test]
fn only_hubd_hub_hubw_or_bun_may_be_killed() {
    let k = |s: &str| is_killable_image(Some(Path::new(s)));
    assert!(k("C:\\b\\hubd.exe"));
    assert!(k("C:\\b\\HUB.EXE"));
    assert!(k("C:\\b\\HubW.exe"));
    assert!(k("C:\\bun\\bun.exe"));
    assert!(!k("C:\\Windows\\System32\\notepad.exe"));
    assert!(!k("C:\\x\\hubd.exe.bak"));
    // Node `basename` ignores a trailing separator.
    assert!(k("C:\\x\\hub.exe\\"));
    assert!(!is_killable_image(None));
    assert_eq!(k("/opt/session-hub/hub"), cfg!(unix));
    assert_eq!(k("/opt/session-hub/HUB"), cfg!(unix));
    assert!(!k("/opt/session-hub/hubd"));
}

// ---------------------------------------------------------------- acquire

#[test]
fn creates_daemon_lock_holding_our_pid_and_start_time_release_deletes_it() {
    let d = tmp();
    let lock = acquire_lock(d.path(), opts(&alive_or_foreign, Some(T0))).unwrap();
    assert!(lock.is_some());
    let file = d.path().join(LOCK_FILE);
    assert_eq!(LOCK_FILE, "daemon.lock");
    assert_eq!(read(&file), format!("{} {T0}", me()));
    drop(lock);
    assert!(!file.exists());
}

#[test]
fn without_a_start_time_it_writes_the_pid_only() {
    let d = tmp();
    let lock = acquire_lock(d.path(), opts(&alive_or_foreign, None)).unwrap();
    assert_eq!(read(&d.path().join(LOCK_FILE)), me().to_string());
    lock.unwrap().release();
    assert!(!d.path().join(LOCK_FILE).exists());
}

#[test]
fn refused_while_a_live_process_holds_it() {
    let d = tmp();
    let first = acquire_lock(d.path(), opts(&alive_or_foreign, None)).unwrap();
    assert!(first.is_some());
    assert!(
        acquire_lock(d.path(), opts(&alive_or_foreign, None))
            .unwrap()
            .is_none()
    );
    drop(first);
    let again = acquire_lock(d.path(), opts(&alive_or_foreign, None)).unwrap();
    assert!(again.is_some());
}

#[test]
fn a_stale_lock_from_a_dead_pid_is_taken_over() {
    let d = tmp();
    let file = d.path().join(LOCK_FILE);
    std::fs::write(&file, dead_pid().to_string()).unwrap();
    let lock = acquire_lock(d.path(), opts(&alive_or_foreign, None)).unwrap();
    assert!(lock.is_some());
    assert_eq!(read(&file), me().to_string());
}

#[test]
fn a_lock_naming_a_foreign_process_is_stale_and_taken_over_the_identity_sees_the_parsed_lock() {
    let d = tmp();
    let file = d.path().join(LOCK_FILE);
    std::fs::write(&file, format!("1234 {T0}")).unwrap();
    let seen = Mutex::new(Vec::new());
    let identify = |l: &LockInfo| {
        seen.lock().unwrap().push(*l);
        Foreign
    };
    let lock = acquire_lock(d.path(), opts(&identify, Some(T0 + 5))).unwrap();
    assert!(lock.is_some());
    assert_eq!(
        *seen.lock().unwrap(),
        [LockInfo {
            pid: 1234,
            started_at: Some(T0)
        }]
    );
    assert_eq!(read(&file), format!("{} {}", me(), T0 + 5));
}

#[test]
fn two_starts_racing_for_a_stale_lock_the_loser_never_deletes_the_winners_fresh_lock() {
    let d = tmp();
    let file = d.path().join(LOCK_FILE);
    std::fs::write(&file, format!("1234 {T0}")).unwrap();
    let rival = "777 1763000000999";
    // While this process judges the stale lock, a rival takes it over (unlink + create).
    let identify = |_: &LockInfo| {
        std::fs::remove_file(&file).unwrap();
        std::fs::write(&file, rival).unwrap();
        Foreign
    };
    let lock = acquire_lock(d.path(), opts(&identify, Some(T0 + 5))).unwrap();
    assert!(lock.is_none());
    assert_eq!(read(&file), rival);
}

#[test]
fn a_lock_held_by_our_daemon_or_by_a_process_that_cannot_be_verified_is_refused() {
    let d = tmp();
    let file = d.path().join(LOCK_FILE);
    std::fs::write(&file, format!("1234 {T0}")).unwrap();
    assert!(
        acquire_lock(d.path(), opts(&|_| Ours, None))
            .unwrap()
            .is_none()
    );
    assert!(
        acquire_lock(d.path(), opts(&|_| Unknown, None))
            .unwrap()
            .is_none()
    );
    assert_eq!(read(&file), format!("1234 {T0}"));
}

#[test]
fn release_leaves_a_lock_that_another_pid_now_holds() {
    let d = tmp();
    let lock = acquire_lock(d.path(), opts(&alive_or_foreign, None)).unwrap();
    let file = d.path().join(LOCK_FILE);
    std::fs::write(&file, "424242").unwrap();
    drop(lock);
    assert_eq!(read(&file), "424242");
}

#[test]
fn release_matches_our_lock_after_trimming() {
    let d = tmp();
    let lock = acquire_lock(d.path(), opts(&alive_or_foreign, Some(T0))).unwrap();
    let file = d.path().join(LOCK_FILE);
    std::fs::write(&file, format!("{} {T0}\r\n", me())).unwrap();
    drop(lock);
    assert!(!file.exists());
}

fn set_mtime(p: &Path, t: SystemTime) {
    std::fs::OpenOptions::new()
        .write(true)
        .open(p)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

#[test]
fn garbage_content_in_an_old_lock_file_is_treated_as_stale() {
    let d = tmp();
    let file = d.path().join(LOCK_FILE);
    std::fs::write(&file, "not-a-pid").unwrap();
    set_mtime(&file, SystemTime::now() - Duration::from_secs(60));
    let lock = acquire_lock(d.path(), opts(&alive_or_foreign, None)).unwrap();
    assert!(lock.is_some());
    assert_eq!(read(&file), me().to_string());
}

#[test]
fn a_fresh_unparseable_lock_may_still_be_mid_write_and_is_held() {
    let d = tmp();
    let file = d.path().join(LOCK_FILE);
    std::fs::write(&file, "").unwrap();
    let never = |_: &LockInfo| -> DaemonIdentity { unreachable!("nothing to identify") };
    assert!(
        acquire_lock(d.path(), opts(&never, None))
            .unwrap()
            .is_none()
    );
    assert_eq!(read(&file), "");
}

#[test]
fn a_missing_folder_is_an_error_not_held() {
    let d = tmp();
    let r = acquire_lock(&d.path().join("missing"), opts(&alive_or_foreign, None));
    assert!(r.is_err());
}
