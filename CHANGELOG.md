# Changelog

All notable changes to Session Hub are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Each entry ends with its issue.

## [Unreleased]

State on 2026-11-15: the CLI and TUI (`hub`), the daemon (`hubd`) and the Windows tray app list the
Claude Code sessions on this PC with their status and seven days of history, and relay the owner's
messages to a session with SHA-256 delivery checks. First public release on 2026-11-14.

### Added

- Role catalogue in `roles.json`: optional `folder`, `brief`, `model`, `effort`, `gateway`, `mcpServers`, `plugins`,
  `telegramChats` and a unique `questionPrefix` per role, plus `reservedPrefixes`, checked by the parser and returned
  by `hub roles --json` with per-role problems (missing paths, a chat two roles list). (#34)
- Mail between sessions, addressed by role and stored in `hub.db` with no model in between: `hub mail send`,
  `inbox`, `follow` (one JSON line per message, for a monitor), `ack`, `done`, `sent` and `show`; roles from an
  optional local `roles.json` or the session's name, and `hub roles`. Mail for a role with no live session waits
  for the next one; the daemon requeues mail of ended sessions, expires old mail and tells the sender about unacked
  or expired mail. The owner's decision list: `hub ask`, `hub answer` (stored once, mailed to the role, a second
  different answer is a conflict) and `hub decisions`. An open decision unanswered for 30 days is closed and its
  role told, so retention removes it 7 days later. Index schema v6. (#28)
- The decision list in the tray: open decisions (and answered ones with a conflict) at the top of the Home and
  Waiting tabs, with options, recommendation, an answer box and Replace after a conflict, plus notices for roles
  whose mail waits for a session; a read-only Decisions tab (key `4`) in the terminal app. Tray answers go through
  `hub bridge` once, are never retried, and are refused when the bridge runs inside a Claude Code session; open
  decisions count in the Waiting badge and the tray icon. The tray is started without the Claude Code session
  variables. (#31)
- Waiting items the owner already answered in the session's own terminal are marked answered and left
  out of the counts; owner messages typed while a session is busy are indexed; the tray has a Send button
  and Dismiss/Undo per item. (#14)
- `bun run relay-eval`: real-model relay check in `default`, `bypassPermissions` and `plan` modes with a
  report, and `relay-eval --check` as a release gate in the Windows test report. (#9)
- CI: a Windows job running the full suite and the Windows-only suite, and a Semgrep scan. (#13)
- The tray build fails on SQL injection, file path injection and disabled token validation findings
  (CA3001, CA3003, CA5404). (#23)

### Changed

- Waiting items already answered are hidden in the tray and the TUI instead of shown dimmed; an answered
  item whose answer box still holds text stays in view. (#18)
- `hub uninstall` deletes the data folder by default; `--keep-data` keeps it. (#11)
- The relay treats its input as data only, may use SendMessage alone, stops after two turns, inherits an
  allow-listed environment and runs in safe mode with a spend cap. (#12, #14)

### Fixed

- The daemon tests date their transcript fixtures from the test clock; fixed dates fell out of the
  seven-day window and were purged, failing ten tests from 2026-11-20. (#27)
- After `/compact`, the compaction summary and local command output were indexed as the owner's
  messages, so open waiting items could show as answered; they are no longer indexed, and an index
  update removes the ones already stored. (#20)
- A send stayed pending, and was counted as a stray relay message, when the relay changed the line break
  after the header line. (#9)
- The answered-items query scanned every message on each session-list read. (#15)
- `ensurePrivateDir` left explicit entries for other principals on the data folder. (#13)

### Security

- Security fix. (#36)
- Security fix. (#33)
- Security fix. (#22)
- Purged message text no longer stays readable in the SQLite WAL after a purge. (#10)
