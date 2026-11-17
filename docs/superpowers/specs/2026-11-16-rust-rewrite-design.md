# Session Hub in Rust: design

Status: approved by the owner, 2026-11-16, including the four "Ask before" items it names (crates, `rust.yml`, the
Unix relay environment names, HKCU Run values with arguments). Tracks issue #8 (ravindu0823/session-hub).

## Intent

Session Hub today needs Bun 1.4.2 and .NET 10 to build, a .NET Desktop Runtime to run, and works only on Windows 11.
Rewrite it in Rust so a release is native code with no runtime to install, built by CI, small and quick to start, and
usable on macOS and Linux where the platform allows. Windows 11 stays the primary target.

This is a port, not a redesign. Every user-visible behaviour and every guarantee in `README.md` and `CLAUDE.md` carries
over unless this document names the change.

## Goals

- Feature parity with the TypeScript daemon, CLI and TUI and with the C# tray app (panel, window, answer boxes, drafts,
  notifications, dismiss, icon states, daemon restart).
- No Bun, Node or .NET at build or run time. The `claude` CLI stays a runtime dependency (it was already one:
  `claude agents --json` for the live list and `claude -p` for the relay).
- Windows 11 first; macOS second; Linux best effort (TUI and daemon fully, tray where a status-notifier host exists).
- CI release builds for Windows, macOS and Linux.
- Ship in slices: each PR is useful on its own, and the Rust parts can run next to the TypeScript/C# parts until the
  last slice removes them.

## Non-goals

- New features. Anything not in today's app waits until parity ships.
- Auto-update, a package-manager listing (winget, Homebrew), code signing. (Release artefacts are unsigned zips/tarballs
  with SHA-256 sums, like today's manifest.)
- "Jump to the session's terminal" on macOS and Linux in the first release (Windows only; other OSes show
  "not supported on this OS yet").
- Changing the relay's behaviour, the delivery rules, the answered-item rules, or the masking rules.

## Anti-goals (a failure even if it works)

- Reading anything under `~/.claude*` that the TypeScript version does not read, or reading more fields of it.
- Any network listener, including a Unix domain socket or named pipe that other processes can connect to.
- Message text in any log.
- A path that answers a permission prompt.
- A console window flashing at Windows login.
- Reporting "delivered" on anything weaker than today's exact SHA-256 match.
- Automatic relay retries.

## Constraints

All of `CLAUDE.md` "Security rules", "Domain rules" and "Ask before" stay binding. In particular:

- Profiles: `~/.claude` and `~/.claude-personal` only. From `sessions/<pid>.json` only `name`, `sessionId`, `status`,
  `updatedAt`, `pid`, `cwd`. Never open, read or stat `*.key`; never connect to `messagingSocketPath`.
- Never store or log `api-request`, `api-request-blob`, attachment bodies, `file-history-*`, tool results.
- Never load whole transcripts: read new bytes from stored offsets only.
- Relay recipients come only from the live list re-read right before the send; at most 6 relays start per minute;
  never retried.
- CI runner images pinned (`windows-2025`, `macos-15`, `ubuntu-24.04`), never `-latest`. Actions pinned by SHA.
- Commits: author `DFanso <leogavin123@outlook.com>`, plain messages, no AI co-author trailer or footer anywhere
  (commits, PR bodies). Work happens in the fork `DFanso/session-hub`, PRs go to `ravindu0823/session-hub` referencing
  #8; nothing is merged upstream by us.
- Tests use the repo's fixtures and synthetic files only, never live sessions or the owner's data.

## Approaches considered

**Tray GUI scope** (decided: full parity in egui).
1. Full parity in egui/eframe: pure Rust, one binary, every tray feature. Biggest, keeps the tray the main place to
   reply. **Picked.**
2. Slim tray (icon, count, menu, notifications, "Open hub" launches the TUI). Much smaller, loses click-to-answer.
3. HTML panel in a system webview (`wry`): closest to today's look, but Linux needs WebKitGTK, so not a single binary.

**Process model** (decided: A).
- **A. Today's process split, one codebase. Picked.** `hub` (CLI + TUI), `hub daemon run` (indexer), `hub tray` (tray).
  One binary on macOS/Linux. On Windows the same code is built twice: `hub.exe` (console subsystem) and `hubw.exe`
  (windows subsystem, used for autostart of daemon and tray, so no console flashes). The tray reads `hub.db` and sends
  through the shared library, so the `hub bridge` child process and its NDJSON protocol go away. Tray Quit leaves the
  daemon running; the tray restarts a crashed daemon (at once, 5 s, 30 s, 2 min; 2 min uptime resets; skipped when
  `run/daemon.stopped` exists).
- B. Daemon inside the tray (`hub` + `hubd`). One fewer process, but Quit stops indexing, a GUI crash kills the indexer,
  headless machines need a no-tray mode. Breaks two documented rules.
- C. One windows-subsystem exe that attaches to the parent console. Prompt returns before output; TUI glitches in some
  terminals.

**Repo layout** (decided with A): a Cargo workspace under `crates/` next to the existing `src/` and `gui/`. Same
`hub.db` schema (v5) and same data directory, so the Rust TUI can read what the TypeScript daemon writes and vice versa.
The last slice deletes `src/`, `gui/`, `package.json`, `bun.lock` etc.

## Architecture

### Crates

| Crate | Kind | Holds | OS calls |
| --- | --- | --- | --- |
| `hub-core` | lib | paths/config, discovery (agents list, sessions watch), ingest (tail, classify), status (extract, answered), security (mask, clean text), store (schema, migrations, queries, purge), send (relay, verify, refusals, rate limit), view (snapshot), daemon (collector, lock, log) | none directly: only through traits in `hub-core::platform` |
| `hub-platform` | lib | `windows`, `macos`, `linux` (shared `unix`) implementations of the platform traits | yes, all of them |
| `hub-tui` | lib | ratatui panes: session list, Status, History, Send | terminal only |
| `hub-tray` | lib | tray icon, menu, notifications, egui panel and window, `gui.json`, daemon supervisor | via `hub-platform` and the GUI crates |
| `hub` | bin crate | argument parsing, wiring, `main`; two bin targets on Windows (`hub`, `hubw`) | — |

`hub-core` has no dependency on any GUI or terminal crate and is the only crate the coverage gate applies to in full.
`hub-tray` is behind a cargo feature `tray` (on by default) so a headless Linux build can skip GTK.

### Platform traits (`hub-core::platform`)

Ported from `src/platform/types.ts` and widened for the tray and install:

- Data-dir privacy: ensure/verify the data dir is private. Windows: today's icacls check (port as-is). Unix: `0700`,
  owned by the current uid, refuse otherwise.
- Process info: alive?, start time, executable path (for lock staleness and kill-guards). `sysinfo` everywhere.
- Process tree kill: Windows `taskkill /T`; Unix a new process group per child + `killpg`.
- Autostart set/clear: Windows HKCU Run; macOS LaunchAgents plists in `~/Library/LaunchAgents`; Linux XDG autostart
  `.desktop` files in `~/.config/autostart`.
- Focus a session's window: Windows only (port of today's parent-walk + `SetForegroundWindow`); others return
  `Unsupported`.
- Tray single-instance / quit / show signals: Windows the same named mutex and events as today
  (`Local\SessionHub.Tray.Instance|Quit|Show`, current-user ACL). Unix: `flock` on `run/tray.lock` for instance;
  quit = `SIGTERM` to the pid in `run/tray.pid` after verifying its executable path; show = `SIGUSR1`. No sockets.
- Hidden spawn: `CREATE_NO_WINDOW` on Windows; detached + new session on Unix.

### Data directory

| OS | Path |
| --- | --- |
| Windows | `%LOCALAPPDATA%\SessionHub\` (unchanged) |
| macOS | `~/Library/Application Support/SessionHub/` |
| Linux | `$XDG_DATA_HOME/session-hub/` (default `~/.local/share/session-hub/`) |

Contents unchanged: `hub.db`, `bin/`, `run/` (`daemon.lock`, `daemon.log`, `relay.log`, `tray.pid`, `tray.log`,
`daemon.stopped`), `relay/`, `gui.json`. `gui.json` keeps today's camelCase format so drafts and settings carry over.

### What ports 1:1 (behaviour identical, tested against the existing fixtures)

- **Discovery:** `claude agents --json` per profile with `CLAUDE_CONFIG_DIR`, 10 s timeout, 4 MiB cap, status mapping,
  own-relay filter; `sessions/` folder watch via `notify` (500 ms debounce) triggering a liveness pass.
- **Ingest:** offsets, 20 MiB first-index window, first-uuid replacement check, 4 MiB chunks / 8 MiB per pass, every
  classification rule in `src/ingest/records.ts` (types dropped, ignored, unknown counters).
- **Status extraction and answered detection:** every rule in `src/status/*`.
- **Masking:** all 23 secret rules, card (network + Luhn), IBAN (mod-97), key-value rule; `[masked:<label>]` output;
  idempotent. Patterns needing look-around use `fancy-regex` with a backtrack limit, or hand-written scanners.
- **Store:** schema v5 exactly (tables, FTS5 external content with secure-delete, triggers, indexes, pragmas, WAL
  required), the migrations array (new migrations append), purge and VACUUM schedule, search, history paging,
  `transitionSend` rules. `rusqlite` with bundled SQLite (FTS5 on).
- **Daemon:** 2 s tail tick, 30 s liveness + recent-transcript scan, single-flight passes, per-profile backoff
  (2/4/8/16/30 s), `stale_since`, lock file `<pid> <startedAtMs>` with today's staleness rules (image list gains
  `hubw.exe`, `hub` on Unix), JSON-lines logs rotated at 5 MiB, `daemon.stopped` handling, `expireSends` every 30 s.
- **Send:** same refusals in the same order, same texts, rate limit and in-flight check inside an IMMEDIATE
  transaction, same relay argv, same stdin format, same success test (`DONE`), same SHA-256 verify, held, expired,
  stray accounting, `relay.log` fields.
- **View snapshot:** same shape and totals, `cleanText` on every string.
- **TUI:** same layout, panes, keys (`↑/↓`, `1/2/3`, `/`, `j`, `r`, `q`, PgUp/PgDn, End, Enter/`e`, Ctrl+S, `y/n/o`,
  Esc), same refresh (1 s) and draft prompt on quit.
- **Tray:** icon states and colours, count + "99+", tooltip ≤127 chars, menu (Open panel, Open window, Quit with
  confirm while a send is open), panel tabs (Home, Sessions, Waiting, Send), window (Status, History, Send), groups
  (Waiting, Working, Idle, Ended), answer form text format and labels, balloon gate (5 min per session, not on first
  snapshot, merged, toggle), dismiss keys, drafts (7-day expiry, debounced atomic writes, `.bad` handling).
- **CLI:** `hub`, `hub daemon run|start|stop|status` (exit codes 0/1/2/3), `hub install [--allow-unreviewed]`,
  `hub uninstall [--keep-data]`, `hub --version`, `hub --help`, plus new `hub tray`.
- **Install:** verified-build manifest (commit + SHA-256 per binary, `dirty` refused without the flag), staged copies,
  rollback, tray quit then daemon stop, shim, autostart, start both.

### What changes

| Change | Why |
| --- | --- |
| `hub bridge` and the NDJSON protocol are removed; the tray reads `hub.db` (polls `data_version` every 500 ms like the bridge did) and calls the send code in-process | One codebase in one language; no child process to supervise |
| Windows exes: `hub.exe` + `hubw.exe` replace `hub.exe`, `hubd.exe`, `SessionHubTray.exe` | Process model A |
| **HKCU Run values** keep their names but gain arguments: `SessionHubDaemon` = `"<bin>\hubw.exe" daemon run`, `SessionHubTray` = `"<bin>\hubw.exe" tray` | `hubw.exe` serves both roles. Needs owner approval (CLAUDE.md "Ask before") |
| **Relay environment allow-list** gains Unix names: `HOME`, `USER`, `LOGNAME`, `SHELL`, `TMPDIR`, `LANG`, `LC_ALL`, `XDG_RUNTIME_DIR`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME` (still no `ANTHROPIC_*`, `CLAUDE_CODE_*`). The argv and everything else stay identical | The relay must find the login on macOS/Linux. Needs owner approval |
| Unix shim: symlink `~/.local/bin/hub` → `<data>/bin/hub` (refuse if a foreign file exists); Windows keeps `hub.cmd` | Unix equivalent of today's shim |
| Windows uninstall self-delete keeps the detached `cmd.exe` loop; Unix deletes in place (a running binary can be unlinked) | OS difference |
| `--render-check` becomes `hub tray --render-check <dir>` using egui's offscreen rendering (`egui_kittest` if it fits; else a snapshot of the egui output) | Keep a headless visual check in CI |

### Crates to add (CLAUDE.md "Ask before": name, publisher, licence, reason)

Versions as of 2026-11-16; exact pins in `Cargo.toml`, `Cargo.lock` committed, builds use `--locked`.

| Crate | Version | Publisher / repo | Licence | Reason |
| --- | --- | --- | --- | --- |
| rusqlite (feature `bundled`) | 0.43.0 | rusqlite org | MIT | SQLite with FTS5, WAL; replaces `bun:sqlite` |
| serde, serde_json | 1.0.229, 1.0.154 | serde-rs (dtolnay) | MIT OR Apache-2.0 | JSONL records, agents list, gui.json, relay output |
| regex | 1.13.1 | rust-lang | MIT OR Apache-2.0 | status extraction, masking (linear time) |
| fancy-regex | 0.20.2 | fancy-regex org | MIT | the few masking/answered patterns that need look-around |
| sha2 | 0.11.1 | RustCrypto | MIT OR Apache-2.0 | delivery verification, manifest, snapshot hash |
| notify | 8.2.2 (stable; 9 is RC) | notify-rs | CC0-1.0 | sessions folder watch |
| time | 0.3.55 | time-rs | MIT OR Apache-2.0 | ISO timestamps |
| dirs | 7.0.0 | dirs-rs (codeberg) | MIT OR Apache-2.0 | home / data dirs per OS |
| sysinfo | 0.40.3 | GuillaumeGomez | MIT | process start time, exe path, alive |
| windows-sys | 0.61.2 | Microsoft | MIT OR Apache-2.0 | registry, mutex/events, focus, `CREATE_NO_WINDOW` (Windows only) |
| rustix | 1.1.6 | bytecodealliance | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | flock, signals, process groups (Unix only) |
| ratatui, crossterm | 0.30.2, 0.29.0 | ratatui org, crossterm-rs | MIT | terminal UI |
| eframe / egui | 0.37.0 | emilk | MIT OR Apache-2.0 | tray panel and window |
| tray-icon | 0.26.1 | tauri-apps | MIT OR Apache-2.0 | notification-area icon and menu |
| notify-rust | 4.19.0 | hoodie | MIT OR Apache-2.0 | "waiting on you" notifications |
| lexopt | 0.3.2 | blyxxyz | MIT | small argument parser |
| anyhow, thiserror | 1.0.104, 2.0.21 | dtolnay | MIT OR Apache-2.0 | errors |
| tempfile (dev only) | 3.27.0 | Stebalien | MIT OR Apache-2.0 | test dirs |

Transitive dependencies are checked by `cargo deny` (licences allow-list: MIT, Apache-2.0, BSD-2/3, ISC, Zlib, CC0,
Unicode-3.0, MPL-2.0 for non-vendored only; advisories deny).

## Testing

- Rust tests mirror today's `test/` areas. They read the existing fixtures in place (`test/fixtures/records`,
  `replies`, `secrets`); fixtures move to `crates/hub-core/tests/fixtures` in the last slice.
- **Parity goldens:** before porting each pure module (classify, extract, answered, mask, snapshot), generate expected
  outputs from the TypeScript implementation over every fixture with a one-off Bun script, commit them as JSON
  goldens, and assert the Rust output equals them byte for byte. This is the evidence for "behaviour identical".
- SQLite: tests on a temp DB; a cross-compat test opens a DB created by the TypeScript schema SQL and runs the Rust
  queries on it.
- Coverage: `cargo llvm-cov`, 85% lines / 75% functions per file for `hub-core` (same bar as today); `hub-platform`,
  `hub-tui`, `hub-tray` UI code and `main` excluded like `src/platform/windows` and `src/tui` are today.
- Windows-only suite behind an env flag, like `SESSION_HUB_WINDOWS_SUITE=1`, run in the `windows` CI job.
- Relay: unit tests with a fake `claude`; `relay-eval` (real relays to throwaway `sh-eval-*` sessions) is ported in the
  send slice and stays a manual pre-release step.

## CI

New workflow `.github/workflows/rust.yml` (the existing `ci.yml` keeps running until the last slice):

- `rust-check` (`ubuntu-24.04`): `cargo fmt --check`, `cargo clippy --locked -- -D warnings`, `cargo test --locked`,
  coverage gate, `cargo deny check`, gitleaks (existing step).
- `rust-windows` (`windows-2025`), `rust-macos` (`macos-15`): `cargo test --locked`; Windows also runs the
  Windows-only suite headless.
- `release` (on tag `v*` only): build `x86_64-pc-windows-msvc`, `aarch64-apple-darwin`, `x86_64-apple-darwin`,
  `x86_64-unknown-linux-gnu`; upload zips/tarballs with the install manifest and SHA-256 sums as workflow artefacts
  and a draft GitHub release. No publishing anywhere else.
- Semgrep job gains the `p/rust` pack.
- Toolchain pinned in `rust-toolchain.toml` (1.98.0). Actions pinned by SHA.

## Slices (one PR each, to `ravindu0823/session-hub`, referencing #8)

1. **Core + daemon + read-only TUI.** Workspace, `hub-core` (everything except send), `hub-platform` for the daemon's
   needs, `hub daemon run|start|stop|status`, TUI Status/History/search, `rust.yml` without `release`. Usable next to
   the TypeScript tray.
2. **Send.** Relay, verify, refusals, rate limit, TUI Send pane, `relay-eval` port.
3. **Tray on Windows.** egui panel and window, icon, notifications, drafts, dismiss, daemon supervisor, render check.
4. **Install, uninstall, autostart (Windows) + release builds.**
5. **macOS:** tray, LaunchAgents, install; Linux XDG autostart and tray best effort.
6. **Remove TypeScript and C#.** Delete `src/`, `gui/`, Bun/.NET files and the old CI jobs; move fixtures; rewrite
   README, CLAUDE.md for the Rust layout and commands.

## Calls made after the builder check (marked as the designer's calls, open to correction)

- **Branching and PRs.** Slices are stacked branches in the fork (`feat/rust-core`, `feat/rust-send`, ...). CI runs on
  the fork's own Actions on every push (`rust.yml` triggers on `feat/**`). Slice 1 is opened as a PR to upstream
  `main`; later slices are opened upstream once their predecessor is merged, and until then live as PRs inside the
  fork whose base is the previous slice's branch. Upstream review changes are rebased down the stack.
- **Which daemon runs during slices 1–3.** The installed TypeScript daemon and C# tray stay what runs at login. The Rust
  daemon is for manual testing only, started by hand after `hub daemon stop` on the TypeScript side (the shared lock
  file stops two daemons running at once). No TypeScript code changes. The switch-over happens at slice 4 through
  `hub install`.
- **Release gate.** `scripts/test-windows-report.ps1` and `scripts/acceptance-gui.ps1` are ported to drive
  `hub.exe`/`hubw.exe` (report in slice 4, GUI acceptance in slice 3) and stay the Windows release gate together with
  `relay-eval` in each profile. macOS and Linux releases need green CI plus a written manual smoke checklist run once
  per release (in `docs/release-checklist.md`, added in slice 5).
- **Claude Code on macOS/Linux.** Slice 5 starts with a feasibility spike: confirm `claude agents --json`, the
  `sessions/` fields, the transcript layout and peer delivery on macOS, using only throwaway sessions created for the
  check (never existing sessions), with the owner's go-ahead at that time. If a part differs, fixtures gain Unix
  variants; if a part is missing, that feature is declared unsupported on that OS in the README.
- **macOS packaging.** The release ships `hub` (CLI/TUI) as a bare binary and the tray as `Session Hub.app`
  (`LSUIElement`, bundle id for notifications) containing the same binary, ad-hoc signed in CI (no Developer ID). The
  README documents clearing quarantine for a browser download. Revisited in slice 5 if the spike shows otherwise.

## Left to the builder

- Module boundaries inside `hub-core`, async vs threads (threads + channels are enough; no async runtime is expected).
- How the egui panel is positioned next to the tray icon on each OS, as long as it matches today's behaviour on Windows.
- The exact `fancy-regex` vs hand-scanner split in masking, as long as goldens pass and no input takes super-linear time.
- Whether `hub-tui` and `hub-tray` share view-model code in `hub-core::view` (preferred) or keep their own.
