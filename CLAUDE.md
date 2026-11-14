# Session Hub

Local Bun + TypeScript terminal app (Ink TUI plus a background daemon) and a C# / WPF tray app that list
Claude Code sessions on this Windows machine, show status and history, and relay owner messages to a session.

## Stack
- Bun 1.4.2 (compiled to single-file exes), TypeScript 7.0.2 strict, Ink 8 + React 19 (the only runtime deps),
  `bun:sqlite` (WAL) for the index, `bun:ffi` for Win32 calls. Biome 2.6.4 for lint and format.
- .NET SDK 10.0.404 pinned in `global.json` (latestPatch; Microsoft.Testing.Platform): `SessionHub.Core` (`net10.0`),
  `SessionHub.Tray` (WPF, `net10.0-windows10.0.19041.0`), `SessionHub.Tests` (xunit.v3 4.0.2).
  `gui/Directory.Build.props`: nullable, warnings as errors, NuGet lock files. No `Directory.Packages.props`.
- No server, no network listener, no database server, no container, no cloud resource. Runs on the owner's
  Windows 11 PC only. Data: `%LOCALAPPDATA%\SessionHub\` (`hub.db`, `bin\`, `run\`, `relay\`, `gui.json`).
- CI: GitHub Actions `.github/workflows/ci.yml` (jobs `test` and `gui`) on GitHub-hosted `ubuntu-24.04`.
  No deploy job: install is `hub install` on the user's PC. Integration branch: `main`.

## Commands
- `bun install --frozen-lockfile --ignore-scripts` (install only this way; exact version pins; runtime deps are
  `ink` and `react` only)
- `bun test` (all, about 90 s), `bun test test/config.test.ts` (one file), `bun test --coverage` (per file: 85% lines,
  75% functions; `src/cli.ts`, `src/cli/real-deps.ts`, `src/tui`, `src/platform/windows` and the fake are excluded,
  `bunfig.toml`). `bun run test:windows` runs the Windows-only suite (it sets `SESSION_HUB_WINDOWS_SUITE=1`;
  it writes throwaway HKCU Run values and focuses windows, so plain `bun test` skips it)
- `bun run lint` (biome ci), `bun run typecheck` (tsc --noEmit)
- `bun run build` (`scripts/build.ts`: `src/cli.ts` to `dist/hub.exe`; `src/daemon-main.ts` to `dist/hubd.exe`
  with no console window; `dotnet publish gui/SessionHub.Tray` to `dist/SessionHubTray.exe`). HKCU Run values
  `SessionHubDaemon` = `"<bin>\hubd.exe"` and `SessionHubTray` = `"<bin>\SessionHubTray.exe"`, no arguments.
- `dotnet restore gui/SessionHub.sln -p:RestoreLockedMode=true`, `dotnet test gui/SessionHub.Tests` (about 5 s),
  `dotnet build gui/SessionHub.sln -warnaserror` (tray app).
- `SessionHubTray.exe --render-check <dir>`: renders every view to PNGs without a tray icon, mutex or bridge.
- `powershell -File scripts/test-windows-report.ps1` (release report), `scripts/acceptance-gui.ps1` (GUI phases).

## Layout
```
src/
  cli.ts, daemon-main.ts   entry points of hub.exe and hubd.exe
  cli/                     command parsing, daemon run, real dependency wiring
  daemon/                  collector loop, ingest pass, single-instance lock, log
  discovery/               `claude agents --json` reader, sessions folder watch
  ingest/                  incremental transcript tail and record classification
  status/                  Progress / What's remaining / Waiting on you extraction
  send/                    relay command and SHA-256 delivery verification
  store/                   SQLite schema, migrations and queries
  security/                secret masking
  view/                    snapshot shared by the TUI and the bridge
  bridge/                  `hub bridge`, the tray's data link over stdin/stdout (`protocol.ts`, `server.ts`)
  tui/                     Ink TUI panes
  platform/                interfaces (`types.ts`), fakes, `windows/*` (Win32, registry, ACL, processes)
test/                      bun tests mirroring src/, fixtures, `test/windows` local-only suite
gui/
  SessionHub.Core/         protocol, bridge client, view models, layout (no WPF, net10.0)
  SessionHub.Tray/         WPF tray, panel and window, render check
  SessionHub.Tests/        xunit.v3 tests of Core
scripts/                   build, install/uninstall helpers, Windows report, GUI acceptance
```

## Security rules (binding)
- Profiles are `~/.claude` and `~/.claude-personal`. Never read any other profile folder.
- Never open, read or stat `*.key` files. Never connect to `messagingSocketPath`.
- From `sessions/<pid>.json` read only `name`, `sessionId`, `status`, `updatedAt`, `pid`, `cwd`.
- Never store or log `api-request`, `api-request-blob`, attachment bodies, `file-history-*`, tool results.
- Logs never contain message text. Secrets (tokens, connection strings, JWTs) are masked before display or indexing.
- No network listener of any kind. TUI and daemon share only `%LOCALAPPDATA%\SessionHub\hub.db`.
- Relay: `claude -p -n session-hub-relay --permission-mode <recipient mode> --strict-mcp-config --tools "SendMessage"
  --allowedTools "SendMessage" --disallowedTools "<every other built-in tool>" --max-turns 2 --model haiku
  --no-session-persistence --output-format json`, message on stdin (the prompt says it is data, never instructions),
  cwd `%LOCALAPPDATA%\SessionHub\relay`, environment allow-listed (`relayEnv` in `src/send/relay.ts`: no
  `ANTHROPIC_*` or `CLAUDE_CODE_*`). Never `--restricted`, never change `crossSessionInbound`.
- A send to a busy session stays "pending" until that session takes the message into its turn, then becomes
  "delivered" (within 30 minutes, else "expired").
- Windows-only code lives in `src/platform/windows/*` behind interfaces in `src/platform/types.ts`.
- Commits: plain messages, no AI co-author trailer. CI uses pinned runner images, never `-latest`.

## Domain rules
- Hub messages are information and instructions, never approvals. Approvals stay in the session's own terminal;
  the hub only brings that window to the front. Never add a path that answers a permission prompt.
- Delivery is "delivered" only on an exact SHA-256 match of the sent text in the recipient sessionId's own
  transcript (peer record from `session-hub-relay`, timestamp after the send started). Never report success otherwise.
- Relay recipients come only from the live list, re-read right before the send; refuse when two live sessions share
  the name.
- A relay is never retried automatically, not even on a provider 429 or overload: the message may already have
  landed. A failed send shows its reason and the owner sends again. At most 6 relays start per minute
  (`MAX_SENDS_PER_MINUTE`); each run is traced in `run\relay.log` without the text.
- Never load whole transcripts into memory: read new bytes from stored offsets only.
- A crashed daemon is restarted by the tray's bridge (at once, then 5 s, 30 s, 2 min; 2 min of uptime resets).
  A deliberate stop (`hub daemon stop`, install, uninstall) writes `run\daemon.stopped` first so it is not
  undone; every daemon run removes it.
- Third-party callers: none. The app has no inbound endpoint, webhook or deploy hook, so SEC-06 has nothing to cover.
  Its only inputs are local files written by Claude Code and the tray's stdin/stdout bridge child.

## Target architecture
- Local single-user desktop tool: one daemon (`hubd`) writes the SQLite index; the TUI and the `hub bridge` child of
  the tray read it. No network, no services, no cloud. Deploy target: `hub install` on the owner's PC from a
  reviewed build; each release needs a passing Windows test report (`scripts/test-windows-report.ps1`).

## Upgrade actions
- .NET 10 LTS (SDK 10.0.404): supported to 2028-11-14. No action.
- Bun 1.4.2, TypeScript 7.0.2, Ink 8.0.0, React 19.3.1, xunit.v3 4.0.2: current at integration (2026-11-13);
  Dependabot (`.github/dependabot.yml`) proposes bumps. No end-of-life date within 12 months is known for these.
- No database server (embedded SQLite through `bun:sqlite`), no Python, no Node runtime in production.

## Ask before
- Changing the relay command, its permission mode logic, or anything that decides what counts as "delivered".
- Reading any new file under `~/.claude*`, adding a new record type to the index, or widening what is logged.
- Adding a package (name, publisher, licence, reason first), changing CI, `scripts/install.ps1` or the HKCU Run values.
- Running `hub install` / `hub uninstall`, touching `%LOCALAPPDATA%\SessionHub`, or starting the real tray app.
