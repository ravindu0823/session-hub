//! Test-only stand-in for `claude agents --json`. Behaviour comes from env `FAKE_CLAUDE_MODE`:
//!
//! - `ok` (default): prints the file named by `FAKE_AGENTS_JSON` (or `[]`), exits `FAKE_AGENTS_EXIT` (0)
//! - `garbage`: prints text that is not JSON
//! - `non-array`: prints `{}`
//! - `exit`: prints `FAKE_AGENTS_MARKER` to stdout and stderr, exits `FAKE_AGENTS_EXIT` (1)
//! - `oversize`: prints valid JSON (`[` + 4 MiB of spaces + `]`)
//! - `sleep-forever`: prints nothing and sleeps 30 s; with `FAKE_AGENTS_GRANDCHILD_PID_FILE` it first
//!   starts a grandchild that shares stdout and writes its pid to that file
//! - `echo-config`: prints one agent whose cwd is `CLAUDE_CONFIG_DIR`
//! - `grandchild`: internal, sleeps 30 s
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn print(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

fn sleep_30s() {
    std::thread::sleep(Duration::from_secs(30));
}

fn exit_code(default: i32) -> i32 {
    env("FAKE_AGENTS_EXIT")
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() {
    let mode = env("FAKE_CLAUDE_MODE").unwrap_or_else(|| "ok".into());
    let code = match mode.as_str() {
        "garbage" => {
            print("not json");
            0
        }
        "non-array" => {
            print("{}");
            0
        }
        "exit" => {
            let marker = env("FAKE_AGENTS_MARKER").unwrap_or_default();
            print(&marker);
            eprint!("{marker}");
            exit_code(1)
        }
        "oversize" => {
            print(&format!("[{}]", " ".repeat(4 * 1024 * 1024)));
            0
        }
        "sleep-forever" => {
            if let Some(pid_file) = env("FAKE_AGENTS_GRANDCHILD_PID_FILE") {
                let me = std::env::current_exe().expect("current_exe");
                // Left running on purpose: the test checks that the tree kill ends it.
                #[allow(clippy::zombie_processes)]
                let gc = Command::new(me)
                    .env("FAKE_CLAUDE_MODE", "grandchild")
                    .stdin(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("spawn grandchild");
                std::fs::write(pid_file, gc.id().to_string()).expect("write pid file");
            }
            sleep_30s();
            0
        }
        "grandchild" => {
            sleep_30s();
            0
        }
        "echo-config" => {
            let cwd = env("CLAUDE_CONFIG_DIR").unwrap_or_default();
            let v = serde_json::json!([{ "pid": 1, "sessionId": "echo", "cwd": cwd }]);
            print(&v.to_string());
            0
        }
        _ => {
            let text = env("FAKE_AGENTS_JSON")
                .and_then(|f| std::fs::read_to_string(f).ok())
                .unwrap_or_else(|| "[]".into());
            print(&text);
            exit_code(0)
        }
    };
    std::process::exit(code);
}
