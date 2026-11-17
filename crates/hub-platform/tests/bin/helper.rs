//! Test-only child for hub-platform tests. Modes (arguments):
//!
//! - `sleep <secs>`: sleeps
//! - `echo-env-exit <VAR> <code>`: prints the value of env VAR and exits with `code`
//! - `tree`: starts a grandchild (`sleep 60`, stdio null), prints its pid and a newline, sleeps 60 s
//! - `tree-exit`: like `tree` but exits at once, leaving the grandchild in its process group
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).map(String::as_str).unwrap_or("");
    match arg(0) {
        "sleep" => std::thread::sleep(Duration::from_secs(arg(1).parse().unwrap_or(60))),
        "echo-env-exit" => {
            let v = std::env::var(arg(1)).unwrap_or_default();
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(v.as_bytes());
            let _ = out.flush();
            std::process::exit(arg(2).parse().unwrap_or(0));
        }
        mode @ ("tree" | "tree-exit") => {
            let me = std::env::current_exe().expect("current_exe");
            // Left running on purpose: the test checks that the tree kill ends it.
            #[allow(clippy::zombie_processes)]
            let gc = Command::new(me)
                .args(["sleep", "60"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn grandchild");
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{}", gc.id());
            let _ = out.flush();
            drop(out);
            if mode == "tree" {
                std::thread::sleep(Duration::from_secs(60));
            }
        }
        other => {
            eprintln!("platform-helper: unknown mode {other:?}");
            std::process::exit(2);
        }
    }
}
