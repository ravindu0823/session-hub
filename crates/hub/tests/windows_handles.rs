//! Windows only: `start` makes the std handles non-inheritable for the spawn and puts them back.
#![cfg(windows)]
use std::path::Path;
use std::sync::Arc;

use hub::cli::CliDeps;
use hub_core::config::{current_os, resolve_config};
use hub_core::discovery::RealClaudeCli;
use windows_sys::Win32::Foundation::{
    GetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};

fn std_flags() -> Vec<u32> {
    [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
        .into_iter()
        .map(|id| unsafe {
            let h = GetStdHandle(id);
            let mut f = 0u32;
            if h.is_null() || h == INVALID_HANDLE_VALUE || GetHandleInformation(h, &mut f) == 0 {
                u32::MAX
            } else {
                f & HANDLE_FLAG_INHERIT
            }
        })
        .collect()
}

#[test]
fn start_restores_the_std_handles_inherit_flags() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().to_str().unwrap().to_string();
    let hub = Path::new(env!("CARGO_BIN_EXE_hub"));
    let fake = hub.with_file_name("fake-claude.exe");
    assert!(fake.exists(), "run `cargo test --workspace`");
    // This is the only test of this binary, so changing the process env cannot race another.
    // SAFETY: single-threaded at this point; the daemon it starts must use the temp dirs.
    unsafe {
        for n in ["HOME", "USERPROFILE"] {
            std::env::set_var(n, &home);
        }
        std::env::set_var("LOCALAPPDATA", format!("{home}\\AppData\\Local"));
    }
    let config = resolve_config(&|n| std::env::var(n).ok(), current_os()).unwrap();
    let platform = hub_platform::native();
    let deps = CliDeps {
        config: Ok(config),
        cli: Arc::new(RealClaudeCli {
            exe: fake,
            platform: platform.clone(),
        }),
        platform,
        exe: hub.to_path_buf(),
        out: Box::new(|_| {}),
        err: Box::new(|_| {}),
    };

    // Make sure at least one std handle is inheritable, so the test can see a change.
    let stdout = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    let usable = !stdout.is_null() && stdout != INVALID_HANDLE_VALUE;
    let original = std_flags();
    if usable {
        unsafe { SetHandleInformation(stdout, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
    }
    let before = std_flags();

    let code = hub::daemon_cmd::start(&deps);
    let after = std_flags();

    let stop = hub::daemon_cmd::stop(&deps);
    if usable {
        unsafe { SetHandleInformation(stdout, HANDLE_FLAG_INHERIT, original[1]) };
    }
    assert_eq!((code, stop), (0, 0));
    assert_eq!(before, after);
    if usable {
        assert_eq!(after[1], HANDLE_FLAG_INHERIT);
    }
}
