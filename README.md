<h1 align="center">Session Hub</h1>

<p align="center">
  <strong>One place to see every Claude Code session on your Windows PC, and to answer the ones waiting on you.</strong>
</p>

<p align="center">
  <a href="https://github.com/ravindu0823/session-hub/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/ravindu0823/session-hub/actions/workflows/ci.yml/badge.svg?branch=main"></a>
  <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-blue.svg"></a>
  <img alt="Platform: Windows 11" src="https://img.shields.io/badge/platform-Windows%2011-0078D6.svg">
  <img alt="Bun 1.4" src="https://img.shields.io/badge/Bun-1.4-000000.svg">
  <img alt=".NET 10" src="https://img.shields.io/badge/.NET-10-512BD4.svg">
</p>

<p align="center">
  <img src="docs/images/tray-panel.png" alt="The tray panel: two sessions waiting on you, one with questions, one with a permission prompt" width="320">
  &nbsp;&nbsp;
  <img src="docs/images/window-history.png" alt="The window: session list on the left, the selected session's history on the right" width="560">
</p>

When you run several Claude Code sessions at once, it is easy to miss the one that has been waiting on you for
twenty minutes. Session Hub watches the sessions on this machine and shows, in one terminal app and one tray icon:

- **What needs you now.** Each session's latest *Progress*, *What's remaining* and *Waiting on you* sections, with the
  questions listed one by one.
- **Who is busy, idle or blocked.** Live state per session, including sessions stuck on a permission prompt.
- **What was said.** The last 7 days of each session's history, searchable, with secrets masked.
- **A way to reply.** Type answers next to each question, or a free message, and send it to that session.
- **A way to approve.** Approvals never go through the hub: one click jumps to the session's own terminal.

Everything runs locally. Session Hub opens no network port and reads only Claude Code's own session files.

## Contents

- [How it works](#how-it-works)
- [Requirements](#requirements)
- [Install](#install)
- [Use it](#use-it): [terminal app](#terminal-app) · [tray app](#tray-app) · [sending a message](#sending-a-message)
- [Security and privacy](#security-and-privacy)
- [Data and retention](#data-and-retention)
- [Troubleshooting](#troubleshooting)
- [Development](#development)
- [License](#license)

## How it works

```mermaid
flowchart LR
    CC["Claude Code sessions<br/>(~/.claude, ~/.claude-personal)"] -- "session files and transcripts (read only)" --> D["hubd<br/>daemon"]
    D -- "masked index" --> DB[("hub.db<br/>SQLite")]
    DB --> TUI["hub<br/>terminal app"]
    DB --> BR["hub bridge"]
    BR -- "stdin / stdout" --> TRAY["Tray app<br/>(WPF)"]
    TUI -- "send" --> R["relay: claude -p<br/>SendMessage only"]
    TRAY -- "send" --> BR
    BR --> R
    R -- "message" --> CC
```

- **`hubd`** (the daemon) tails each session's transcript from where it left off, extracts the status sections, masks
  secrets and writes a SQLite index. It starts at login.
- **`hub`** is the terminal app and the command line. **The tray app** gets the same data through a `hub bridge`
  child process over stdin and stdout.
- **Sending** runs one fixed `claude -p` relay that can only call `SendMessage`. Delivery counts only when the exact
  text (SHA-256) shows up in the recipient's own transcript.

## Requirements

| Needs | Version |
| --- | --- |
| OS | Windows 11 |
| Runtime | .NET 10 Desktop Runtime (the tray app is a framework-dependent single file) |
| Claude Code | tested with 2.1.323; profiles read: `~/.claude` and `~/.claude-personal` |
| To build | Bun 1.4.2 and .NET SDK 10.0.404 (pinned in `global.json`) |

## Install

```powershell
bun install --frozen-lockfile --ignore-scripts
bun run build
dist\hub.exe install
```

That is all. `hub install` copies the three programs to `%LOCALAPPDATA%\SessionHub\bin\`, adds a `hub` command,
starts the daemon and the tray, and sets both to start at login. Or run `scripts\install.ps1`, which builds and
installs in one step from a clean, pushed `main`.

To remove it:

```powershell
hub uninstall               # quits the tray, clears autostart, stops the daemon, removes the hub command and the data
hub uninstall --keep-data   # the same, but keeps the data directory
```

<details>
<summary><strong>What the build and install do, in detail</strong></summary>

`bun run build` produces three executables: `hub.exe` (every command, including the TUI), `hubd.exe` (the daemon only,
built without a console window) and `SessionHubTray.exe` (the tray app, published from `gui/SessionHub.Tray`).
`hub install` copies all three to `%LOCALAPPDATA%\SessionHub\bin\`, writes a `hub.cmd` shim to
`%USERPROFILE%\.local\bin` (already on PATH), creates the private data directory, sets two HKCU Run values with no
arguments (`SessionHubDaemon` = `"%LOCALAPPDATA%\SessionHub\bin\hubd.exe"`, `SessionHubTray` =
`"%LOCALAPPDATA%\SessionHub\bin\SessionHubTray.exe"`), then starts the daemon and the tray. It never edits PATH, and it
refuses to install if a foreign `hub.cmd` already exists in that folder.

**Verified builds.** `bun run build` also writes `dist\install-manifest.txt`: the commit it built from (marked `dirty`
when the tree had uncommitted changes) and the SHA-256 of each exe. `hub install` refuses exes that do not match it,
and a `dirty` build unless it gets `--allow-unreviewed`. It stages and checks all three copies before it replaces any
exe, puts the old exes back if a replace fails part-way, prints the commit, and keeps the manifest in `bin\` so the
installed build can be traced. `scripts\install.ps1` builds and installs in one step, and only from a clean tree on
`main` whose HEAD is already on `origin/main`, unless it gets `-AllowUnreviewed`.

**Replacing a running install.** A running tray is asked to quit first (it saves typed drafts), so `hub install` can
replace the exes. If it has not exited after 5 s it is stopped by force, but only when `run\tray.pid` names a process
running `bin\SessionHubTray.exe`. If an exe cannot be replaced (retried for 10 s), the installed daemon and tray are
started again and `hub install` exits with 1.

**Elevated trays.** A tray started elevated ("Run as administrator") cannot be quit or stopped by a `hub install` or
`hub uninstall` run from a normal prompt: Windows lets a normal process neither set the elevated tray's quit event nor
end it. `hub install` then stops with an error saying the tray did not quit or did not stop, before it replaces or
registers anything; `hub uninstall` reports the same and removes the rest. Quit that tray from its tray-icon menu,
then run the command again.

</details>

## Use it

### Terminal app

Run `hub`. It starts the daemon if it is not running.

| Key | Action |
| --- | --- |
| `↑` / `↓` | Select a session |
| `1` / `2` / `3` | Status / History / Send pane |
| `/` | Search |
| `j` | Jump to the session's terminal window |
| `r` | Refresh |
| `q` | Quit |

Daemon commands: `hub daemon run|start|stop|status`. `hub daemon start` runs `hubd.exe` hidden in the background;
`hub daemon run` runs the daemon in the current console. `hub --version` prints the version.

### Tray app

`SessionHubTray.exe` sits in the notification area and starts at login. Left-click the icon to open or close the
panel next to the taskbar; right-click for Open panel, Open window and Quit.

| Icon | Meaning |
| --- | --- |
| Neutral | Daemon running, nothing waiting |
| Orange with a count | That many items are waiting on you |
| Grey | The daemon is stopped; the panel offers "Start daemon" |
| Red | The tray cannot reach `hub.exe` (three failed starts in a row) |

The monitor button in the panel opens the window: a resizable two-pane view with Status, History and Send for the
selected session.

<details>
<summary><strong>Tray and window keys</strong></summary>

Panel:

| Key | Action |
| --- | --- |
| `Esc` | Close the panel |
| `Ctrl+1` to `Ctrl+4` | Home / Sessions / Waiting / Send tab |
| `Ctrl+F` | Search |
| `Enter` | Send the reply |
| `Shift+Enter` | New line in the reply |

Window:

| Key | Action |
| --- | --- |
| `Ctrl+1` / `Ctrl+2` / `Ctrl+3` | Status / History / Send tab (Send puts the cursor in the reply box) |
| `Ctrl+F` | Search: the History search on the History tab, else the session list search |
| `Esc` | Clear the search box it is typed in |
| `Enter` / `Shift+Enter` | Send the reply / new line |

</details>

⚙ Notifications in the panel turns the "waiting on you" balloons on or off (at most one per session every 5 minutes).
Settings and drafts are kept in `%LOCALAPPDATA%\SessionHub\gui.json`. Quit closes the tray only; the daemon keeps
running. Start the tray again from `%LOCALAPPDATA%\SessionHub\bin\SessionHubTray.exe` or at the next login.

### Sending a message

<p align="center">
  <img src="docs/images/window-answers.png" alt="The Send tab: an answer box under each waiting question, a free message box and a preview of what the session will receive" width="560">
</p>

Each question a session is waiting on gets its own answer box, plus one box for a free message. The preview shows
exactly what the session will receive. In the terminal app: type the text, `Ctrl+S` shows the preview (recipient
name, profile, cwd and the full message), `y` sends, `n` cancels. If an earlier send to that session is still open,
the pane shows its state and `o` is needed to override.

| State | Meaning |
| --- | --- |
| pending | The session is busy; the message waits until it takes it into its turn |
| held | The session cannot take it yet: approve in the session's terminal |
| delivered | The exact text was found in the recipient's transcript |
| expired | Not delivered within 30 minutes |
| failed | The relay reported an error; nothing is retried automatically, send again |

## Security and privacy

- **No network listener of any kind.** The terminal app and the daemon share only the SQLite file `hub.db`. The tray
  gets its data from a `hub.exe bridge` child over stdin and stdout.
- **Minimal reading.** From `sessions/<pid>.json` the hub reads only `name`, `sessionId`, `status`, `updatedAt`, `pid`
  and `cwd`. It never opens, reads or stats `*.key` files and never connects to the Claude Code messaging socket. It
  never stores API request bodies, attachment bodies, file history or tool results.
- **Private data directory.** `%LOCALAPPDATA%\SessionHub` is restricted to your account with an ACL. Secrets (tokens,
  connection strings, JWTs), card numbers and IBANs are masked before they are shown or indexed. Logs hold counts,
  timings, session names and errors, never message text.
- **Approvals stay in the terminal.** Hub messages are information and instructions, never approvals. The hub cannot
  approve a permission prompt; it only brings that window to the front.
- **Your message leaves the machine.** The relay is one Claude Haiku turn under the recipient profile's Claude Code
  login, so the text goes to Anthropic as typed, under that account's data terms. Text that looks like a secret or a
  card number is refused until you confirm it again. At most 6 relays start per minute, and each run is traced in
  `run\relay.log` without the text.

<details>
<summary><strong>The relay command and process safeguards, in detail</strong></summary>

Sending runs one fixed relay command, with the message on stdin and cwd `%LOCALAPPDATA%\SessionHub\relay`:

    claude -p -n session-hub-relay --permission-mode <recipient mode> --strict-mcp-config --tools "SendMessage" --allowedTools "SendMessage" --model haiku --no-session-persistence --output-format json

The relay runs in the recipient's current permission mode (from its latest permission-mode record), which can be
`bypassPermissions`. Its tools are limited to `SendMessage`. It never uses `--restricted` and never changes
`crossSessionInbound`. A relay message that matches no send to the session it landed in is counted
(`relay_stray_count`) and logged.

The tray's only other signals are two named events that carry no data and only the current user can open:
`Local\SessionHub.Tray.Quit` (set by `hub install` and `hub uninstall`) and `Local\SessionHub.Tray.Show` (a second
start opens the running tray's panel). To stop the tray, `hub install` and `hub uninstall` also open the tray's
instance mutex `Local\SessionHub.Tray.Instance` (SYNCHRONIZE only, to see whether a tray still runs) and read the
executable path of the process named in `run\tray.pid` (PROCESS_QUERY_LIMITED_INFORMATION), so they never kill a
process that is not `bin\SessionHubTray.exe`.

The daemon is guarded the same way: `run\daemon.lock` holds its pid and its process start time, so a pid that Windows
gave to another process after an unclean logoff (a different start time) is a stale lock, never reported as the
daemon or killed. A live pid whose start time cannot be read (for example an elevated daemon) still counts as
running, so no second daemon starts, but it is never killed; a kill also needs the image to be `hubd.exe`, `hub.exe`
or `bun.exe`. A lock from an older version (pid only) is judged by the process image instead.

</details>

To report a vulnerability, use GitHub's private vulnerability reporting on this repository.

## Data and retention

Everything lives in `%LOCALAPPDATA%\SessionHub\`:

| Item | What it holds | How long |
| --- | --- | --- |
| `hub.db` | Session list, parsed status sections and the masked message text of each session, with a full-text index | Messages older than 7 days are deleted from every session, live ones included; a session's row goes 7 days after it ends. The daemon purges hourly and compacts the file daily |
| `gui.json` | Tray settings and unsent drafts | A draft is dropped when its session has not been seen for 7 days |
| `run\` | Daemon and tray logs, `relay.log`, lock and pid files | Logs never contain message text |
| `bin\` | The installed programs and `install-manifest.txt` | Until `hub uninstall` |

`hub uninstall` deletes the data directory too; `hub uninstall --keep-data` keeps it.

## Troubleshooting

| Symptom | What to do |
| --- | --- |
| Nothing updates | `hub daemon status` prints `RUNNING pid=<n>` or `STOPPED`; if stopped, run `hub daemon start` |
| Need the logs | Daemon: `%LOCALAPPDATA%\SessionHub\run\daemon.log`; tray: `run\tray.log`. Neither contains message text |
| "Format changed" banner in the Status pane | Claude Code wrote records the hub does not recognise; compare your Claude Code version with the tested one |
| No sessions listed | Check that Claude Code is running and that `~/.claude` or `~/.claude-personal` exists |

Tested with Claude Code 2.1.323 on Windows 11. Each release needs a passing Windows report from
`scripts/test-windows-report.ps1`.

## Development

```powershell
bun install --frozen-lockfile --ignore-scripts
bun test                       # all tests; `bun test --coverage` enforces 85% lines per file
bun run test:windows           # Windows-only tests
bun run lint                   # biome ci
bun run typecheck              # tsc --noEmit
bun run build                  # dist/hub.exe, dist/hubd.exe and dist/SessionHubTray.exe
dotnet test gui/SessionHub.Tests                   # tray app logic tests
powershell -File scripts/test-windows-report.ps1   # Windows release report
```

| Path | What lives there |
| --- | --- |
| `src/daemon`, `src/ingest`, `src/status` | Collector loop, transcript tailing, status extraction |
| `src/store`, `src/security` | SQLite schema and migrations, secret masking |
| `src/send` | Relay command and SHA-256 delivery verification |
| `src/bridge`, `src/tui`, `src/cli` | Tray data link, terminal app, commands |
| `gui/` | Tray app: `SessionHub.Core` (no WPF), `SessionHub.Tray` (WPF), `SessionHub.Tests` |

CI (`.github/workflows/ci.yml`) runs lint, typecheck, coverage tests, `bun audit` and gitleaks on the `test` job, and
the tray app's restore, tests, build and publish on the `gui` job, both on GitHub-hosted `ubuntu-24.04`. CodeQL scans
pushes and pull requests to `main`.

## License

MIT. See [`LICENSE`](LICENSE).
