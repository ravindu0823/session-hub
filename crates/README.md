# Rust hub (transition)

The Rust port of Session Hub is built in slices next to the TypeScript and C# app. Until the last
slice, the installed TypeScript daemon and the C# tray stay what runs at login. The Rust hub is for
manual testing only.

## What it replaces so far (slice 1)

- The daemon: discovery (`claude agents --json`, sessions folder watch), transcript ingest, status
  extraction, secret masking, delivery checks and expiry of sends, the `hub.db` index (same file,
  schema v5).
- `hub daemon run|start|stop|status`.
- The TUI, read-only: session list, Status, History and search, `j` to bring a session's terminal
  window to the front.

Not yet: sending (the Send pane only says so), `hub bridge`, the tray, `hub install` / `hub uninstall`
and autostart. They come in later slices.

## Crates

- `hub-core`: everything that is not OS-specific (config, ingest, status, store, daemon, snapshot).
- `hub-platform`: Windows and Unix process, ACL/permission and window code behind the `Platform`
  trait.
- `hub-tui`: the terminal UI.
- `hub`: the `hub` binary and, on Windows, `hubw` (the same commands without a console window).

## Build and test

```
cargo build --locked --release -p hub      # target/release/hub(.exe) and hubw(.exe)
cargo test --locked --workspace
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
scripts/coverage-gate.sh                   # needs cargo-llvm-cov and jq
cargo deny --locked check                  # needs cargo-deny
```

Windows-only suite (changes ACLs of temp folders and focuses windows), in PowerShell:

```
$env:SESSION_HUB_WINDOWS_SUITE = "1"; cargo test --locked -p hub-platform
```

Masking timing tests (release build):

```
cargo test --locked --release -p hub-core --test security -- --ignored mask_is_linear
```

The goldens in `crates/hub-core/tests/goldens` come from the TypeScript code:
`bun scripts/rust-goldens.ts` regenerates them, and CI fails when they are out of date.

## Run

```
hub                     open the TUI (starts the daemon if needed)
hub daemon run          run the daemon in this console
hub daemon start        start the daemon hidden in the background
hub daemon stop         stop the daemon
hub daemon status       RUNNING pid=<n> (exit 0) or STOPPED (exit 3)
hub --version           print the version
hub --help              print this help
```

Run the Rust binary by its path (for example `target\release\hub.exe`), so it is not confused with
the installed TypeScript `hub.exe`. It uses the same data folder (`%LOCALAPPDATA%\SessionHub` on
Windows) and the same `hub.db`.

On Windows, `hub daemon start` runs `hubw.exe daemon run` from the folder of `hub.exe`. Build both
binaries (`cargo build -p hub`); `cargo run --bin hub` builds only `hub.exe`, and without `hubw.exe`
next to it `hub daemon start` runs `hub.exe` itself. `hubw.exe` with no arguments prints nothing and
exits 2.

## Manual testing next to the TypeScript hub

1. Stop the TypeScript daemon with the TypeScript `hub daemon stop`. Both daemons use the same lock
   file (`run\daemon.lock`), so a second daemon does not start while one runs.
2. Start the Rust daemon (`hub daemon run` or `hub daemon start` of the Rust build).
3. Stop it only with the Rust `hub daemon stop`. The TypeScript `hub daemon stop` does not know
   `hubw.exe` and refuses to kill it.
4. Start the TypeScript daemon again (TypeScript `hub daemon start`).

Every daemon run, Rust or TypeScript, removes `run\daemon.stopped`. So while the Rust daemon runs,
the tray no longer knows the daemon was stopped on purpose: if the Rust daemon exits any other way
than the Rust `hub daemon stop` (Ctrl-C in `hub daemon run`, a crash), the tray starts the
TypeScript daemon again.

## Known gaps

- A transcript record nested deeper than 128 levels, or with a number out of range (such as
  `1e999`), cannot be read by `serde_json`; the Rust hub skips that record where TypeScript reads it.
- A session row in `hub.db` that cannot be decoded (for example corrupt status JSON) is left out of
  the list. The daemon logs `sessions_unreadable` with the count, and the TUI shows the count on the
  Status pane; no row values are shown or logged.
- Bringing a terminal window to the front (`j`) works on Windows only.
- The TypeScript side does not support macOS or Linux. The Rust hub builds and its tests run there,
  but Claude Code on those systems is not checked yet (a later slice).
