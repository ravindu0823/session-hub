# Rust slice 1 (core, daemon, read-only TUI) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Rust `hub` binary that runs the indexing daemon (`hub daemon run|start|stop|status`) and a read-only TUI (session list, Status, History, search) on the same `hub.db` the TypeScript daemon writes, with behaviour proven identical to the TypeScript code by goldens.

**Architecture:** Cargo workspace in `crates/` next to `src/` and `gui/`. `hub-core` is a port of `src/` (minus relay, bridge, install, TUI) with OS calls behind a `Platform` trait; `hub-platform` implements it for Windows and Unix; `hub-tui` is ratatui over `hub-core`; `hub` is the bin crate (`hubw` second bin on Windows). Pure modules are verified against JSON goldens produced by running the TypeScript functions over the fixtures and a corpus of the TS tests' own inputs.

**Tech Stack:** Rust 1.98.0 (edition 2024), rusqlite 0.43 (bundled, FTS5), serde/serde_json, regex 1.13 + fancy-regex 0.20, sha2 0.11, notify 8.2.2, time 0.3, dirs 7, sysinfo 0.40, windows-sys 0.61, rustix 1.1, ratatui 0.30 + crossterm 0.29, lexopt 0.3, anyhow/thiserror, tempfile (dev). Bun (any 1.3+) only to regenerate goldens.

**Spec:** `docs/superpowers/specs/2026-11-16-rust-rewrite-design.md` (slice 1). The TypeScript file named in each task is the behavioural spec for that task: port it rule for rule, with its doc comments.

## Global Constraints

- Every rule in `CLAUDE.md` "Security rules" and "Domain rules" applies to the Rust code exactly as to the TS code.
- Profiles `~/.claude` and `~/.claude-personal` only; from `sessions/<pid>.json` only `name`, `sessionId`, `status`, `updatedAt`, `pid`, `cwd`; never open/read/stat `*.key`; never connect to `messagingSocketPath`.
- Never store or log `api-request`, `api-request-blob`, `api-request-shape`, `file-history-*`, attachment bodies, tool results. Logs never contain message text.
- No network listener, socket or pipe of any kind.
- Never load whole transcripts: read new bytes from stored offsets only.
- `hub.db` schema stays v5, byte-identical DDL to `src/store/schema.ts`; a DB with a newer `schema_version` is refused.
- Exact versions in `Cargo.toml` (`=x.y.z`), `Cargo.lock` committed, every cargo command in CI uses `--locked`.
- CI images pinned: `ubuntu-24.04`, `windows-2025`, `macos-15`; actions pinned by full commit SHA with a version comment.
- No changes under `src/`, `gui/`, `test/` (TS/C# keep working and `ci.yml` keeps passing).
- Commits: author `DFanso <leogavin123@outlook.com>`, plain conventional messages, no `Co-Authored-By`, no "Generated with" line. PR body: no AI attribution.
- Tests use fixtures and temp dirs only; nothing reads the real `~/.claude*` or the real data dir. Config is always injected.
- JS → Rust regex translation: JS `\d`, `\w`, `\b` are ASCII; write them as `[0-9]`, `[A-Za-z0-9_]`, `(?-u:\b)` (or disable Unicode for that group). JS `\s` and Rust `\s` both match Unicode whitespace; keep `\s`. Look-around goes to `fancy-regex` with `backtrack_limit` set, or a hand scanner.
- JS string lengths are UTF-16 code units. Wherever TS compares or slices by `.length` (ID ≤ 24, log fields ≤ 200, answered scan ≤ 20,000, snippet sizes), count UTF-16 units (`s.encode_utf16().count()`) and cut only on a `char` boundary at or before the limit. Never panic on a slice.

## Review Focus

1. **Non-ASCII text at length limits** (emoji, CJK, combining marks straddling 24/200/20,000 units): same output as TS, no panic → corpus entries in Tasks 3, 5 and a log clip test in Task 11.
2. **Unicode digits and letters in patterns** (`Progress: ٤٢% overall`, ID `Q١`, full-width `Ｑ1`): not matched, exactly like TS → corpus entries in Task 5.
3. **A multi-byte UTF-8 char split across a read chunk** (4 MiB chunk ends mid-char, or a line is half-written): only complete lines are returned, the char is intact on the next read → test in Task 6.
4. **CRLF transcripts and Windows-style paths** (`C:\\fixture`, `\r\n` line ends, backslash cwd read on macOS): `\r` stripped, paths kept verbatim → tests in Tasks 4 and 6.
5. **DB from a newer build or held by the TS daemon** (`schema_version` 6, one above the supported 5; a writer holding the lock for 2 s): open is refused with "newer schema"; reads wait on `busy_timeout` instead of failing → tests in Task 7.

---

## File Structure

```
Cargo.toml                         workspace: members crates/*, resolver 3, [workspace.dependencies] pins
rust-toolchain.toml                channel 1.98.0, components rustfmt clippy llvm-tools-preview
deny.toml                          licences allow-list, advisories deny, sources crates.io only
.github/workflows/rust.yml         rust-check, rust-windows, rust-macos
.github/workflows/ci.yml           + semgrep p/rust pack, + goldens-up-to-date step in `test`
scripts/rust-goldens.ts            Bun: runs TS functions over fixtures + corpus, writes goldens
scripts/coverage-gate.sh           per-file 85% lines / 75% functions for hub-core
crates/hub-core/
  src/lib.rs                       module tree only
  src/config.rs                    Config, Profile, ProfileId, Os, resolve_config, constants
  src/platform.rs                  Platform trait, FocusResult, FakePlatform (cfg test/feature "fake")
  src/text.rs                      utf16_len, clip_utf16 (shared length helpers)
  src/security/{mod,mask,clean}.rs mask_secrets, looks_secret, clean_text, clean_input
  src/ingest/{mod,records,tail}.rs classify, Classified, TailState, initial_state, read_new
  src/status/{mod,extract,answered}.rs extract_status, Status, WaitingItem, answered_ids
  src/store/{mod,schema,db}.rs     MIGRATIONS, Store and row types
  src/send/{mod,verify}.rs         delivery checker, expire_sends, stray accounting (no relay yet)
  src/discovery/{mod,agents,watch}.rs ClaudeCli trait, RealClaudeCli, parse_agents, watch_sessions
  src/daemon/{mod,log,lock,transcripts,ingest_pass,collector,run}.rs
  src/view/snapshot.rs             HubSnapshot, read_snapshot, clean_session
  tests/goldens/{records,extract,answered,mask,clean}.json, schema.json   (generated)
  tests/corpus/{records,extract,answered,mask,clean}.json                 (hand-assembled inputs)
  tests/*.rs                       one integration test file per module area
crates/hub-platform/src/{lib.rs, windows/{mod,acl,process,focus}.rs, unix/{mod,process,privacy}.rs}
crates/hub-tui/src/{lib.rs, app.rs, list.rs, status_pane.rs, history_pane.rs, format.rs}
crates/hub/src/{main.rs, cli.rs, daemon_cmd.rs}, src/bin/hubw.rs (Windows: #![windows_subsystem="windows"])
```

---

### Task 1: Workspace, config and CI

**Files:**
- Create: `Cargo.toml`, `rust-toolchain.toml`, `deny.toml`, `crates/hub-core/{Cargo.toml,src/lib.rs,src/config.rs,src/text.rs}`, `crates/hub-core/tests/config.rs`, `.github/workflows/rust.yml`, `scripts/coverage-gate.sh`
- Modify: `.github/workflows/ci.yml` (semgrep `--config p/rust`), `.gitignore` (`/target`)

**Interfaces:**
- Produces:
  - `pub enum Os { Windows, MacOs, Linux }`, `pub fn current_os() -> Os`
  - `pub enum ProfileId { Claude, ClaudePersonal }` with `fn as_str(&self) -> &'static str` (`"claude"`, `"claude-personal"`)
  - `pub struct Profile { pub id: ProfileId, pub dir: PathBuf }`, `pub struct Config { pub profiles: Vec<Profile>, pub data_dir: PathBuf }` with `fn run_dir()`, `fn relay_dir()`, `fn db_path()` (`run`, `relay`, `hub.db`)
  - `pub fn resolve_config(env: &dyn Fn(&str) -> Option<String>, os: Os) -> Result<Config, ConfigError>`
  - constants, same values as `src/config.ts`: `DEBOUNCE_MS=500`, `SAFETY_POLL_MS=30_000`, `AGENTS_TIMEOUT_MS=10_000`, `RETENTION_MS=7*86_400_000`, `FIRST_INDEX_BYTES=20*1024*1024`, `DELIVERY_WINDOW_MS=30*60_000`, `EXPIRE_EVERY_MS=30_000`, `RELAY_NAME="session-hub-relay"`, `EVAL_SESSION_PREFIX="sh-eval-"`, `STRAY_WARN_MS=24*3600_000`, `RELAY_LOG="relay.log"`, `DAEMON_STOPPED_FILE="daemon.stopped"`, `MESSAGE_HEADER` (exact string from `src/config.ts`)
  - `text::utf16_len(&str) -> usize`, `text::clip_utf16(&str, max_units: usize) -> &str` (longest prefix ≤ max units ending on a char boundary)

- [ ] **Step 1: Write the failing tests** in `crates/hub-core/tests/config.rs`:
  - `windows_paths`: env `USERPROFILE=C:\Users\a`, `LOCALAPPDATA=C:\Users\a\AppData\Local` → profiles `C:\Users\a\.claude`, `C:\Users\a\.claude-personal`; data dir `C:\Users\a\AppData\Local\SessionHub`.
  - `windows_requires_env`: missing `USERPROFILE` → `Err` whose message contains `USERPROFILE is not set`; same for `LOCALAPPDATA`.
  - `macos_paths`: `HOME=/Users/a` → `/Users/a/.claude`, `/Users/a/.claude-personal`, data `/Users/a/Library/Application Support/SessionHub`.
  - `linux_paths`: `HOME=/home/a` → data `/home/a/.local/share/session-hub`; with `XDG_DATA_HOME=/x` → `/x/session-hub`; a relative `XDG_DATA_HOME` is ignored (XDG spec).
  - `only_two_profiles`: exactly two profiles, ids in that order (`.tclaude` and others never listed).
  - `clip_utf16_boundaries`: `clip_utf16("ab😀c", 3) == "ab"`, `clip_utf16("ab😀c", 4) == "ab😀"`, `utf16_len("😀") == 2`.
  (Paths are built with string joins using the target OS separator so the Windows case runs on macOS: implement `resolve_config` with a separator chosen from `os`, returning `PathBuf::from(String)`.)
- [ ] **Step 2: Run** `cargo test -p hub-core --test config` → fails to compile (no crate).
- [ ] **Step 3: Create the workspace and implement.** Workspace `[workspace.package] edition="2024", rust-version="1.98", license="MIT"`; `[workspace.dependencies]` lists every crate in the spec table with `=` pins (only those used by slice 1); `[workspace.lints.clippy] all="deny"`. `deny.toml`: licences `MIT, Apache-2.0, Apache-2.0 WITH LLVM-exception, BSD-2-Clause, BSD-3-Clause, ISC, Zlib, CC0-1.0, Unicode-3.0`, `MPL-2.0`; advisories `deny`; `unknown-registry = "deny"`, `unknown-git = "deny"`.
- [ ] **Step 4: Run** `cargo test -p hub-core --test config` → PASS. `cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings` → clean.
- [ ] **Step 5: Write `rust.yml`**: triggers `push: [main, "feat/**"]`, `pull_request: [main]`, `workflow_dispatch`; `permissions: contents: read`; concurrency like `ci.yml`.
  - `rust-check` (`ubuntu-24.04`): checkout (same pinned SHA as `ci.yml`), `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked --workspace`, install `cargo-llvm-cov` and `cargo-deny` with `taiki-e/install-action` pinned by SHA, `scripts/coverage-gate.sh`, `cargo deny --locked check`.
  - `rust-windows` (`windows-2025`, env `SESSION_HUB_HEADLESS=1`) and `rust-macos` (`macos-15`): `cargo test --locked --workspace`.
  - The toolchain comes from `rust-toolchain.toml` (rustup on the hosted image reads it).
  - `scripts/coverage-gate.sh`: `cargo llvm-cov --locked -p hub-core --json --summary-only`, then `jq` fails when any file under `crates/hub-core/src` (except `platform.rs`) has `lines.percent < 85` or `functions.percent < 75`, printing each offender.
- [ ] **Step 6: Commit** `git add Cargo.toml Cargo.lock rust-toolchain.toml deny.toml crates scripts/coverage-gate.sh .github .gitignore && git commit -m "build(rust): workspace, config paths per OS, rust CI"`. Push to `fork feat/rust-core` and confirm the three `rust.yml` jobs are green on the fork (`gh run list -R DFanso/session-hub -b feat/rust-core`).

### Task 2: Golden generator and corpus

**Files:**
- Create: `scripts/rust-goldens.ts`, `crates/hub-core/tests/corpus/{records,extract,answered,mask,clean}.json`, generated `crates/hub-core/tests/goldens/*.json`, `crates/hub-core/tests/common/mod.rs`
- Modify: `.github/workflows/ci.yml` (`test` job: step "Rust goldens up to date": `bun scripts/rust-goldens.ts && git diff --exit-code crates/hub-core/tests/goldens`)

**Interfaces:**
- Produces: golden files, each a JSON array of `{"name": string, "input": <input>, "output": <TS result>}`:
  - `records.json`: input = one JSONL line (string); output = `classify(line)`.
  - `extract.json`: input = markdown; output = `extractStatus(md)` (null allowed).
  - `answered.json`: input = `{listed: string[], texts: string[]}`; output = sorted array of `answeredIds(...)`.
  - `mask.json`: input = text; output = `maskSecrets(text)`.
  - `clean.json`: input = `{fn: "text"|"input", s: string, meta?: bool}`; output = string.
  - `schema.json`: `SELECT type,name,tbl_name,sql FROM sqlite_master ORDER BY name` of a fresh `Store.open(tmp, {create:true})`, plus `PRAGMA user_version` / `schema_version` row value.
  - `tests/common/mod.rs`: `pub fn golden(name: &str) -> Vec<GoldenCase>` with `GoldenCase { name: String, input: serde_json::Value, output: serde_json::Value }`; `pub fn fixture(rel: &str) -> PathBuf` resolving against `<workspace>/test/fixtures`.

- [ ] **Step 1: Assemble the corpus.** For each area copy every input literal from the TS tests into the corpus file, named `<ts-test-file>:<test title>[#n]`: `test/ingest/records.test.ts` → records; `test/status/extract.test.ts` → extract; `test/status/answered.test.ts` → answered; `test/security/mask.test.ts`, `mask-more.test.ts` → mask (skip the timing generators; they become Task 3's linear test); `test/tui/sanitize.test.ts` → clean. Add the Review Focus inputs, named `focus:<n>`:
  - mask/extract/answered: emoji and CJK straddling 24 units in an ID line (`Q12345678901234567890😀: x`), `Progress: ٤٢% overall. Phase: x.`, `Q١: yes`, `Ｑ1: yes`, a 20,001-unit answered text whose ID sits at the end, combining mark at the 200-unit clip.
  - records: one CRLF-terminated line (`...}\r`), one with `"cwd":"C:\\\\Program Files\\\\x"`.
  The script also feeds every line of `test/fixtures/records/*.jsonl`, every `test/fixtures/replies/*.md`, every `test/fixtures/secrets/*.txt` (named `fixture:<file>[:<line>]`).
- [ ] **Step 2: Write `scripts/rust-goldens.ts`** importing `classify`, `extractStatus`, `answeredIds`, `maskSecrets`, `cleanText`, `cleanInput`, `Store`, writing pretty JSON (2 spaces, trailing newline, stable key order as produced) so `git diff` is clean on regeneration.
- [ ] **Step 3: Run** `bun scripts/rust-goldens.ts` twice → second run leaves `git status` clean (deterministic).
- [ ] **Step 4: Write `common/mod.rs`** and a test `goldens_load` asserting every golden file parses and is non-empty. Run `cargo test -p hub-core --test goldens_load` → PASS.
- [ ] **Step 5: Commit** `git commit -m "test(rust): TS goldens generator, corpus and loader"`.

### Task 3: Masking and terminal-safe text

**Files:**
- Create: `crates/hub-core/src/security/{mod.rs,mask.rs,clean.rs}`, `crates/hub-core/tests/security.rs`
- Port from: `src/security/mask.ts`, `src/tui/sanitize.ts`

**Interfaces:**
- Produces: `pub fn mask_secrets(text: &str) -> String`, `pub fn looks_secret(text: &str) -> bool` (`mask_secrets(t) != t`), `pub fn clean_text(s: &str) -> String`, `pub fn clean_input(chunk: &str, meta: bool) -> String`.

- [ ] **Step 1: Write the failing tests:**
  - `mask_matches_goldens`: for every case in `golden("mask")`, `mask_secrets(input) == output` (failure message names the case).
  - `clean_matches_goldens`: same for `golden("clean")`, dispatching on `fn`.
  - `mask_is_idempotent`: for every mask golden, `mask_secrets(&output) == output`.
  - `mask_is_linear`: port `expectLinear` from `test/security/mask.test.ts` with the same input generators, the same 8x size / 24x ratio gate, best of ≥3 alternating rounds, up to 7 extra rounds; the absolute 1 s cap is skipped when `SESSION_HUB_NO_ABS_TIMING=1`. Mark `#[ignore]` in debug builds and run it in CI with `cargo test --release -p hub-core --test security -- --ignored mask_is_linear` (add that step to `rust-check`).
- [ ] **Step 2: Run** `cargo test -p hub-core --test security` → FAIL (unresolved).
- [ ] **Step 3: Implement.** Rule order and labels exactly as `mask.ts` (output `[masked:<label>]`). Card: network prefix + length + Luhn, then the card-shaped windows longest first; IBAN mod-97 on the digit-expanded string with incremental remainder (no big ints). `fancy-regex` only for the patterns with look-around, `RegexBuilder::backtrack_limit(100_000)`; on a backtrack-limit error treat that rule as "no match" for that span and continue (log nothing: this is a pure function). Compile all regexes once (`std::sync::LazyLock`).
- [ ] **Step 4: Run** `cargo test -p hub-core --test security` and the release-mode linear test → PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(rust): secret masking and terminal-safe text, matching TS goldens"`.

### Task 4: Transcript record classification

**Files:**
- Create: `crates/hub-core/src/ingest/{mod.rs,records.rs}`, `crates/hub-core/tests/records.rs`
- Port from: `src/ingest/records.ts`

**Interfaces:**
- Produces:
```rust
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", rename_all_fields = "camelCase")]
pub enum Classified {
    OwnerPrompt { uuid: String, ts: String, text: String },
    PeerMessage { uuid: String, ts: String, from_name: String, from_mode: String, body: String },
    AssistantText { uuid: String, ts: String, text: String },
    HeldNotice { uuid: String, ts: String, peer_name: String },
    PermissionMode { ts: String, mode: String },
    Ignored { reason: String, #[serde(rename = "type", skip_serializing_if = "Option::is_none")] record_type: Option<String> },
    Unknown { #[serde(rename = "type")] record_type: String, #[serde(skip_serializing_if = "Option::is_none")] version: Option<String> },
}
pub fn classify(line: &str) -> Classified;
```
  (Serialised form equals the TS object, so goldens compare as `serde_json::Value`.)

- [ ] **Step 1: Write the failing tests:** `records_match_goldens` (`serde_json::to_value(classify(input)) == output` for every case); `never_store_types_carry_no_content`: for each of `api-request`, `api-request-blob`, `api-request-shape`, `file-history-snapshot`, `file-history-delta` with a body containing `SECRET-BODY`, `format!("{:?}", classify(..))` does not contain `SECRET-BODY`.
- [ ] **Step 2: Run** `cargo test -p hub-core --test records` → FAIL.
- [ ] **Step 3: Implement** with `serde_json::Value` navigation mirroring the TS checks in order (sidechain, peer origin, isMeta, non-human origin, content shape, tool_result, empty). The peer body is cut between the first `<cross-session-message` open tag and the last `</cross-session-message>`, trimming one newline each end; attributes `from-name`, `from-mode` parsed as in TS.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(rust): transcript record classification"`.

### Task 5: Status extraction and answered items

**Files:**
- Create: `crates/hub-core/src/status/{mod.rs,extract.rs,answered.rs}`, `crates/hub-core/tests/status.rs`
- Port from: `src/status/extract.ts`, `src/status/answered.ts`

**Interfaces:**
- Produces:
  - `#[serde(rename_all="camelCase")] pub struct WaitingItem { pub id: Option<String>, pub text: String, pub is_new: bool, pub children: Vec<String> }`
  - `#[serde(rename_all="camelCase")] pub struct Status { pub progress_pct: Option<i64>, pub phase: Option<String>, pub remaining: Vec<String>, pub waiting: Vec<WaitingItem>, pub recommendations: Vec<String> }` (Serialize + Deserialize; stored as JSON columns in Task 7)
  - `pub fn extract_status(markdown: &str) -> Option<Status>`
  - `pub fn answered_ids(listed: &[String], texts: &[String]) -> BTreeSet<String>`

- [ ] **Step 1: Write the failing tests:** `extract_matches_goldens`, `answered_matches_goldens` (output compared as a sorted `Vec<String>`).
- [ ] **Step 2: Run** `cargo test -p hub-core --test status` → FAIL.
- [ ] **Step 3: Implement**, translating each TS regex per Global Constraints (`\d` → `[0-9]`, case-insensitive kept, curly quote `\u{2019}`). `progress_pct`: parse as f64, `round()` like JS `Math.round` (halves toward +∞: use `(x + 0.5).floor()`), clamp 0–100. ID length cap 24 counts UTF-16 units. Answered scan cap 20,000 UTF-16 units per message via `clip_utf16`.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(rust): status extraction and answered-item detection"`.

### Task 6: Incremental transcript tail

**Files:**
- Create: `crates/hub-core/src/ingest/tail.rs`, `crates/hub-core/tests/tail.rs`
- Port from: `src/ingest/tail.ts`; tests from `test/ingest/tail.test.ts`

**Interfaces:**
- Produces:
  - `#[serde(rename_all="camelCase")] pub struct TailState { pub path: String, pub offset: u64, pub size: u64, pub first_uuid: Option<String> }`
  - `pub fn initial_state(path: &Path, max_first_bytes: u64) -> io::Result<TailState>`
  - `pub struct ReadNew { pub lines: Vec<String>, pub next: TailState, pub reset: bool }`
  - `pub fn read_new(state: &TailState, max_bytes: u64) -> io::Result<ReadNew>` (`max_bytes == 0` → `io::ErrorKind::InvalidInput`)

- [ ] **Step 1: Write the failing tests**, one Rust test per test in `test/ingest/tail.test.ts` with the same file contents and expectations, plus:
  - `crlf_lines_lose_cr`: file `"{\"uuid\":\"a\"}\r\n{\"uuid\":\"b\"}\r\n"` → lines `{"uuid":"a"}`, `{"uuid":"b"}`.
  - `split_multibyte_char_waits`: write `{"t":"` + `"é"` where the read budget ends between the two UTF-8 bytes of `é` and no newline yet → `lines` empty, offset unchanged; append `"}\n` → next read returns the full line with `é` intact.
  - `partial_last_line_not_returned`: `"a\nb"` → lines `["a"]`, offset 2.
  - `first_read_aligns_after_window`: a 25 MiB file with `FIRST_INDEX_BYTES` window starts after the first newline at or after `size - 20 MiB`.
  - `replaced_file_resets` (first uuid in first 64 KB differs) and `shrunk_file_resets`.
- [ ] **Step 2: Run** `cargo test -p hub-core --test tail` → FAIL.
- [ ] **Step 3: Implement** with `File::seek` + `read` of at most `max_bytes`, splitting on `b'\n'` and decoding each complete line with `String::from_utf8_lossy` only after the split (bytes past the last newline are never decoded).
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(rust): incremental transcript tail from stored offsets"`.

### Task 7: SQLite store

**Files:**
- Create: `crates/hub-core/src/store/{mod.rs,schema.rs,db.rs}`, `crates/hub-core/tests/store.rs`
- Port from: `src/store/schema.ts` (copy `MIGRATIONS` SQL strings verbatim), `src/store/db.ts`; tests from `test/store/db.test.ts`

**Interfaces:**
- Consumes: `Status`, `WaitingItem` (Task 5), `TailState` (Task 6), `answered_ids` (Task 5), `RELAY_NAME`, `RETENTION_MS`.
- Produces (`camelCase` serde on all row types; field names = TS names in snake_case):
  - `SessionRow { session_id, profile, name, cwd, pid: i64, kind, status: LiveStatus, started_at: i64, updated_at: i64, ended_at: Option<i64>, transcript_path, permission_mode: Option<String> }`
  - `pub enum LiveStatus { Busy, Idle, Waiting, Unknown }` (serde lowercase) — defined here, reused by Task 9.
  - `MessageRow { uuid, session_id, ts: String, role: Role, from_name: Option<String>, text }`, `pub enum Role { Owner, Peer, Assistant }`
  - `SendRow { id: Option<i64>, session_id, profile, name, sha256, created_at: i64, state: SendState, detail: Option<String> }`, `pub enum SendState { Pending, Held, Delivered, Expired, Failed }`
  - `ViewWaitingItem { #[serde(flatten)] item: WaitingItem, answered: bool }`, `StoredStatus { progress_pct, phase, remaining, waiting: Vec<ViewWaitingItem>, recommendations, reply_uuid, reply_ts }`, `LatestReply { reply_uuid, reply_ts, has_status }`, `SessionView { #[serde(flatten)] row fields except status, state: LiveStatus, status: Option<StoredStatus>, latest_reply: Option<LatestReply>, waiting_count: i64 }`, `SearchHit { session_id, uuid, ts, role, snippet }`
  - `pub struct Store` with `open(path: &Path, create: bool) -> Result<Store, StoreError>`, `open_read_only(path) -> Result<Store, StoreError>`, and methods mirroring `db.ts` one for one: `close`, `vacuum`, `upsert_session`, `mark_ended`, `get_session`, `list_sessions`, `get_tail`, `save_tail`, `add_messages`, `history(session_id, limit, before: Option<(&str, &str)>)`, `search(q, limit)`, `set_status`, `set_latest_reply`, `purge(now) -> usize`, `create_send`, `transition_send(id, to, detail: Option<Option<&str>>) -> bool`, `count_sends_since`, `get_send`, `open_send_for`, `open_sends(Option<&str>)`, `send_hashes_since`, `sends_to_verify`, `set_meta(key, Option<&str>)`, `get_meta`, `data_version`.
  - `StoreError::NewerSchema { found: i64, supported: i64 }` (message: `hub.db has schema version {found}, newer than this build supports ({supported})`), `StoreError::WalUnavailable`, `StoreError::Sqlite(rusqlite::Error)`.

- [ ] **Step 1: Write the failing tests:** one Rust test per test in `test/store/db.test.ts` (same rows, same assertions, including the ANSWERS_SQL query-plan test using `EXPLAIN QUERY PLAN`), plus:
  - `schema_matches_ts`: fresh DB's `sqlite_master` rows and schema version equal `golden("schema")`.
  - `refuses_newer_schema`: set `schema_version` to 6 in a v5 DB → `open` returns `NewerSchema { found: 6, supported: 5 }`.
  - `waits_for_writer`: thread A opens the DB, `BEGIN IMMEDIATE`, sleeps 2 s, commits; thread B's `upsert_session` started during that window succeeds (busy_timeout 5000) and the row is present.
  - `pragmas`: `journal_mode=wal`, `synchronous=1`, `foreign_keys=1`, `secure_delete=1`, `busy_timeout=5000`.
- [ ] **Step 2: Run** `cargo test -p hub-core --test store` → FAIL.
- [ ] **Step 3: Implement** with `rusqlite` (`bundled` feature; FTS5 is included). Migrations in one `BEGIN IMMEDIATE` transaction each, exactly as `Store.migrate`. Purge in batches of 500 (`PURGE_BATCH`), then `wal_checkpoint(TRUNCATE)`; FTS query building identical to `ftsQuery`; `snippet(messages_fts, 0, '[', ']', '...', 12)`.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(rust): hub.db store, schema v5 identical to TS"`.

### Task 8: Delivery checks run by the daemon

The daemon's per-record hook verifies sends the TS tray made. Only the verification half of `src/send/` is in this slice; relaying is slice 2.

**Files:**
- Create: `crates/hub-core/src/send/{mod.rs,verify.rs}`, `crates/hub-core/tests/verify.rs`
- Port from: `src/send/verify.ts`; tests from `test/send/verify.test.ts`

**Interfaces:**
- Consumes: `Store`, `SendRow`, `SendState` (Task 7), `Classified` (Task 4), `MESSAGE_HEADER`, `RELAY_NAME`, `EVAL_SESSION_PREFIX`, `DELIVERY_WINDOW_MS`.
- Produces: `pub fn build_message(text: &str) -> String`, `pub fn normalize(text: &str) -> String`, `pub fn sha256_hex(text: &str) -> String`, `pub const HEADERLESS_DETAIL: &str = "delivered without the Session Hub header"`, `pub fn check_delivery(send: &SendRow, records: &[Classified], now: i64) -> SendState`, `pub fn delivery_checker(now: Arc<dyn Fn() -> i64 + Send + Sync>, log: Arc<dyn Logger>) -> impl FnMut(&Store, &str, &[Classified])` (the store is passed per call: the collector thread owns the only `Store`), `pub fn expire_sends(store: &Store, now: i64) -> usize`.

- [ ] **Step 1: Write the failing tests:** one Rust test per test in `test/send/verify.test.ts`, same records and expectations (delivered only on exact SHA-256 of `normalize(header + "\n\n" + text)`, ts ≥ created − 1 s, headerless accepted with `HEADERLESS_DETAIL`, held only for the newest pending send, one record proves at most one send, strays counted excluding `sh-eval-*`, expired after 30 min, failed → delivered within the window). Add `sha256_matches_ts`: `sha256_hex(&normalize(&build_message("hi\r\n")))` equals the hex the TS test file computes for the same input (copy the literal).
- [ ] **Step 2: Run** `cargo test -p hub-core --test verify` → FAIL.
- [ ] **Step 3: Implement** with `sha2::Sha256`. This task creates `daemon/log.rs` with only the `Logger` trait, `LogFields` and `LogValue` from Task 11's Interfaces (no file logger yet) plus a `MemoryLogger` test double; Task 11 adds `create_logger`.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(rust): delivery verification, expiry and stray accounting"`.

### Task 9: Discovery (live list and sessions watch)

**Files:**
- Create: `crates/hub-core/src/discovery/{mod.rs,agents.rs,watch.rs}`, `crates/hub-core/tests/discovery.rs`, `crates/hub-core/tests/bin/fake_claude.rs` (test-only bin target `fake-claude`)
- Port from: `src/discovery/agents.ts`, `src/discovery/watch.ts`; tests from `test/discovery/*.test.ts`, fake from `test/fakes/fake-claude.ts`

**Interfaces:**
- Consumes: `LiveStatus` (Task 7), `Platform::{spawn_hidden, kill_tree}` (Task 10 defines the trait; this task adds the trait with those two methods to `platform.rs` and a `FakePlatform` that does nothing).
- Produces:
  - `#[serde(rename_all="camelCase")] pub struct LiveSession { pub pid: i64, pub cwd: String, pub kind: String, pub started_at: i64, pub session_id: String, pub name: String, pub status: LiveStatus }`
  - `pub trait ClaudeCli: Send + Sync { fn agents(&self, profile_dir: &Path, stop: &StopToken) -> Result<Vec<LiveSession>, AgentsError>; }`
  - `pub struct RealClaudeCli { pub exe: PathBuf, pub platform: Arc<dyn Platform> }` (runs `<exe> agents --json`, env = inherited + `CLAUDE_CONFIG_DIR=<profile_dir>`, stdin/stderr null, timeout `AGENTS_TIMEOUT_MS`, stdout cap `MAX_AGENTS_STDOUT_BYTES = 4 MiB`, kill tree on timeout/stop; error texts as TS: `claude agents aborted`, timeout, non-zero exit, invalid JSON, not an array)
  - `pub fn parse_status(v: &Value) -> LiveStatus`, `pub fn parse_agents(v: &Value) -> Vec<LiveSession>`, `pub fn is_own_relay(s: &LiveSession, relay_dir: &Path) -> bool` (case-insensitive compare on Windows only)
  - `pub fn find_claude(env_path: &OsStr) -> Option<PathBuf>` (PATH search: `claude.exe`, `claude.cmd` on Windows; `claude` elsewhere)
  - `pub fn watch_sessions(profile_dir: &Path, on_change: impl Fn() + Send + 'static) -> Result<WatchHandle, WatchError>`; `WatchHandle` stops on drop; fires on `*.json` (or unknown name) changes in `<profile>/sessions` only, debounced `DEBOUNCE_MS`; never opens any file.
  - `pub struct StopToken` (`Arc<(Mutex<bool>, Condvar)>`): `stop()`, `is_stopped()`, `wait_timeout(Duration) -> bool` (true when stopped).

- [ ] **Step 1: Write the failing tests:** port every test in `test/discovery/agents.test.ts` and `watch.test.ts` (fake claude modes: ok, garbage, non-array, exit 1, sleep-forever for the timeout and stop tests, oversize stdout); plus `watch_never_touches_key_files`: create `sessions/x.key`, change it, assert no callback and (on Linux CI) that `notify` events are the only access — implement by asserting the watcher code path contains no `File::open` (unit-test `should_fire("x.key") == false`, `should_fire("1.json") == true`, `should_fire(None) == true`).
- [ ] **Step 2: Run** `cargo test -p hub-core --test discovery` → FAIL.
- [ ] **Step 3: Implement.** The fake claude is a `[[test]]`-adjacent bin compiled via `env!("CARGO_BIN_EXE_fake-claude")`, behaviour chosen by env `FAKE_CLAUDE_MODE`.
- [ ] **Step 4: Run** → PASS on macOS; push and confirm `rust-windows` passes the same tests.
- [ ] **Step 5: Commit** `git commit -m "feat(rust): live session list via claude agents and sessions folder watch"`.

### Task 10: Platform implementations

**Files:**
- Modify: `crates/hub-core/src/platform.rs` (full trait)
- Create: `crates/hub-platform/{Cargo.toml,src/lib.rs,src/windows/{mod,acl,process,focus}.rs,src/unix/{mod,process,privacy}.rs}`, `crates/hub-platform/tests/platform.rs`
- Port from: `src/platform/windows/{acl,process,exec,focus,sysbin}.ts`, `src/util/taskkill.ts`, `src/util/proc.ts`; tests from `test/platform/{acl-parse,exec,sysbin}.test.ts`, `test/windows/platform.test.ts`

**Interfaces:**
- Produces:
```rust
pub enum FocusResult { Focused { title: String }, NotFound { reason: String }, Unsupported }
pub trait Platform: Send + Sync {
    fn ensure_private_dir(&self, dir: &Path) -> io::Result<()>;
    fn is_private_dir(&self, dir: &Path) -> io::Result<bool>;
    fn process_alive(&self, pid: u32) -> bool;
    fn process_start_time(&self, pid: u32) -> Option<i64>;   // epoch ms
    fn process_image_path(&self, pid: u32) -> Option<PathBuf>;
    fn spawn_hidden(&self, cmd: &mut std::process::Command); // CREATE_NO_WINDOW | own process group
    fn kill_tree(&self, pid: u32) -> io::Result<()>;
    fn focus_session_window(&self, pid: u32) -> FocusResult;
}
pub fn native() -> Arc<dyn Platform>;   // in hub-platform
```
- Windows: ACL via the same `icacls`/`whoami` invocations and parser as `acl.ts` (system32 paths from `sysbin.ts`); process info via `sysinfo`; `kill_tree` via `taskkill /T /F /PID` from system32; focus = port of `focus.ts` (parent walk, `SetForegroundWindow` via `windows-sys`).
- Unix: private dir = mode `0o700` and `st_uid == geteuid()`; `ensure_private_dir` creates with `0o700` and chmods an existing dir it owns; `spawn_hidden` = `process_group(0)` (+ `setsid` for daemon start); `kill_tree` = `killpg(pgid, SIGKILL)`; `focus_session_window` → `Unsupported`.

- [ ] **Step 1: Write the failing tests:** port the `acl-parse`, `exec`, `sysbin` tests (pure parsers, run on all OSes); `unix_private_dir_mode` (`#[cfg(unix)]`: ensure → mode 0700; a 0755 dir owned by us → `is_private_dir == false`, then `ensure` fixes it); `own_process_info`: for `std::process::id()`, alive, start time within 60 s of now, image path ends with the test binary name; `kill_tree_kills_children` (spawn `sh -c 'sleep 60 & wait'` / `cmd /c ping -n 60 127.0.0.1` hidden, kill tree, child gone within 5 s); `windows_suite` tests from `test/windows/platform.test.ts` behind `SESSION_HUB_WINDOWS_SUITE=1`.
- [ ] **Step 2: Run** `cargo test -p hub-platform` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** → PASS locally; add to `rust-windows` a step `cargo test --locked -p hub-platform` with `SESSION_HUB_WINDOWS_SUITE=1`, push, confirm green.
- [ ] **Step 5: Commit** `git commit -m "feat(rust): platform layer for Windows and Unix"`.

### Task 11: Daemon building blocks (log, lock, transcripts, ingest pass)

**Files:**
- Create: `crates/hub-core/src/daemon/{mod.rs,log.rs,lock.rs,transcripts.rs,ingest_pass.rs}`, `crates/hub-core/tests/{log,lock,transcripts,ingest_pass}.rs`
- Port from: `src/daemon/{log,lock,transcripts,ingest-pass}.ts`; tests from `test/daemon/{log,lock,ingest-pass}.test.ts`

**Interfaces:**
- Consumes: Tasks 3–8, `Platform` (Task 10).
- Produces:
  - `pub type LogFields = Vec<(&'static str, LogValue)>`, `pub enum LogValue { Num(f64), Bool(bool), Str(String) }`, `pub trait Logger: Send + Sync { fn info(&self, event: &str, f: LogFields); fn warn(..); fn error(..); }`, `pub fn create_logger(dir: &Path, max_bytes: u64, file: &str) -> Arc<dyn Logger>` (`LOG_MAX_BYTES = 5 MiB`, rotation `.1`, `.2`, string fields clipped to 200 UTF-16 units, line = `{...fields, "ts", "level", "event"}`), `pub fn err_fields(e: &dyn std::error::Error) -> LogFields`
  - `pub const LOCK_FILE = "daemon.lock"`, `START_TIME_SLACK_MS = 2_000`, `pub struct LockInfo { pid: u32, started_at: Option<i64> }`, `parse_lock`, `format_lock`, `pub enum DaemonIdentity { Ours, Foreign, Unknown }`, `ProcessProbe`, `daemon_identity(lock, probe, dev) -> DaemonIdentity`, `identify_daemon(lock, &dyn Platform, dev)`, `is_killable_image(image: Option<&Path>) -> bool` (basename, case-insensitive: `hubd.exe`, `hub.exe`, `hubw.exe`, `bun.exe`, and `hub` on Unix), `acquire_lock(dir, LockOptions) -> Option<LockGuard>` (`O_EXCL` create, compare-and-delete takeover, guard releases on drop only if content still ours)
  - `first_cwd(path) -> io::Result<String>`, `list_transcripts(profile_dir) -> io::Result<Vec<PathBuf>>`, `is_session_id(&str) -> bool`, `transcript_index(profile_dir) -> io::Result<HashMap<String, PathBuf>>`
  - `READ_CHUNK_BYTES = 4 MiB`, `MIN_READ_BYTES = 64 KiB`, `MAX_BYTES_PER_PASS = 8 MiB`, `MAX_UNRECOGNIZED_TYPES = 10`, `pub struct IngestCtx<'a> { store: &'a Store, get_row: &'a dyn Fn(&str) -> Option<SessionRow>, row_changed: &'a dyn Fn(&SessionRow), now: &'a dyn Fn() -> i64, on_records: Option<&'a mut dyn FnMut(&Store, &str, &[Classified])> }`, `pub struct IngestResult { lines, messages, unknown, unrecognized: usize, more: bool }`, `pub fn ingest_file(ctx, session_id, path, on_records_error: &dyn Fn(&dyn Error)) -> io::Result<IngestResult>`, `mask_status(&Status) -> Status`, `safe_version(&str) -> String`

- [ ] **Step 1: Write the failing tests:** port every test in the three TS test files. Add:
  - `lock_reads_ts_lock`: content `"1234 1731740000000"` → `LockInfo { pid: 1234, started_at: Some(1731740000000) }`; `"1234"` → legacy; `"x"` → `None`; `format_lock` round-trips byte-identical to TS (`"<pid> <ms>"`, no newline — check TS `formatLock` and copy).
  - `log_never_writes_message_text`: run `ingest_file` over `test/fixtures/records/*.jsonl` with a file logger and an `on_records` that errors; the log file contains none of the fixture `text`/`body` strings.
  - `log_clip_utf16`: a 199-unit ASCII prefix + `😀` field is clipped to the 199 ASCII units.
- [ ] **Step 2: Run** `cargo test -p hub-core --test log --test lock --test transcripts --test ingest_pass` → FAIL.
- [ ] **Step 3: Implement.** One SQLite transaction per batch, as TS; status parsed from the newest assistant text before masking, then masked; timestamps normalised to ISO UTC with ms (`time` crate, format `[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z`), bad ones fall back to the batch time.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(rust): daemon log, lock, transcript listing and ingest pass"`.

### Task 12: Collector loop and `daemon run`

**Files:**
- Create: `crates/hub-core/src/daemon/{collector.rs,run.rs}`, `crates/hub-core/tests/collector.rs`
- Port from: `src/daemon/collector.ts`, `src/cli/daemon-run.ts`; tests from `test/daemon/collector.test.ts`

**Interfaces:**
- Consumes: Tasks 7–11.
- Produces:
  - `TAIL_POLL_MS = 2_000`, `BACKOFF_MS = [2_000, 4_000, 8_000, 16_000, 30_000]`
  - `pub struct DaemonDeps { cli: Arc<dyn ClaudeCli>, store: Store, platform: Arc<dyn Platform>, now: Arc<dyn Fn() -> i64 + Send + Sync>, profiles: Vec<Profile>, data_dir: PathBuf, relay_dir: PathBuf, stop: StopToken, watch: WatchFn, log: Arc<dyn Logger>, daemon_identity: Box<dyn Fn(&LockInfo) -> DaemonIdentity + Send> }` where `type WatchFn = Box<dyn Fn(&Path, Box<dyn Fn() + Send>) -> Result<WatchHandle, WatchError> + Send>`
  - `pub fn run_daemon(deps: DaemonDeps) -> Result<(), DaemonError>` (blocks until `stop`; `DaemonError::AlreadyRunning`, `DaemonError::NotPrivate`)
  - `pub enum RunOutcome { Stopped, AlreadyRunning, Failed(String) }`, `pub fn daemon_run(deps: DaemonRunDeps) -> RunOutcome`: removes `run/daemon.stopped` on start, installs Ctrl-C/SIGTERM (and Windows Ctrl-Break/close) handlers that call `stop.stop()`, runs `expire_sends` every `EXPIRE_EVERY_MS`, wires `delivery_checker` as `on_records`, logs panics through `std::panic::set_hook` then stops cleanly.
  Threads, not async: one collector thread (tick loop on `stop.wait_timeout`), watcher callbacks send on a channel; passes are single-flight and triggers during a pass merge into one follow-up.

- [ ] **Step 1: Write the failing tests:** port every test in `test/daemon/collector.test.ts` using a fake `ClaudeCli` (scripted responses per call), the fake platform, a manual clock and a fake `WatchFn` the test can fire. Timing-based TS tests use the injected clock plus `StopToken`-driven ticks; no test sleeps more than 3 s.
- [ ] **Step 2: Run** `cargo test -p hub-core --test collector` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** → PASS on macOS; push; `rust-windows` and `rust-macos` green.
- [ ] **Step 5: Commit** `git commit -m "feat(rust): collector loop and daemon run"`.

### Task 13: View snapshot

**Files:**
- Create: `crates/hub-core/src/view/{mod.rs,snapshot.rs}`, `crates/hub-core/tests/snapshot.rs`
- Port from: `src/view/snapshot.ts`; tests from `test/view/snapshot.test.ts`

**Interfaces:**
- Consumes: `Store::list_sessions`, `get_meta`, `clean_text`.
- Produces: `#[serde(rename_all="camelCase")] pub struct HubSnapshot { sessions: Vec<SessionView>, meta: SnapshotMeta, totals: Totals }`, `SnapshotMeta { unknown_count: i64, unknown_version: Option<String>, stale: Vec<StaleProfile { profile: String, since: i64 }>, daemon: DaemonState /* "running"|"stopped" */, relay_stray_count: i64 }`, `Totals { waiting_items, needs_approval, live: i64 }`, `pub fn clean_session(s: SessionView) -> SessionView`, `pub fn read_snapshot(store: &Store, daemon_running: bool, known_profiles: &[&str], now: i64) -> HubSnapshot`.

- [ ] **Step 1: Write the failing tests:** port `test/view/snapshot.test.ts`; plus `snapshot_json_shape`: `serde_json::to_value(read_snapshot(..))` of a seeded store has exactly the TS key set at each level (`sessions[0]` keys: `sessionId, profile, name, cwd, pid, kind, state, startedAt, updatedAt, endedAt, transcriptPath, permissionMode, status, latestReply, waitingCount`). This shape is what slice 3's tray consumes.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(rust): hub snapshot for TUI and tray"`.

### Task 14: `hub` binary: daemon commands

**Files:**
- Create: `crates/hub/{Cargo.toml,src/main.rs,src/cli.rs,src/daemon_cmd.rs,src/bin/hubw.rs}`, `crates/hub/tests/cli.rs`
- Port from: `src/cli/commands.ts` (`daemon` and `--version`/`--help` branches only), `src/cli/real-deps.ts` (wiring)

**Interfaces:**
- Consumes: `resolve_config`, `daemon_run`, `hub_platform::native`, `RealClaudeCli`, `find_claude`, lock helpers.
- Produces:
  - `pub fn run(args: Vec<OsString>, deps: &CliDeps) -> i32` (exit codes: 0 ok, 1 failure, 2 usage, 3 stopped); `hub` with no args → TUI (Task 15).
  - `hub daemon status` prints `RUNNING pid=<n>` (exit 0) or `STOPPED` (exit 3); `hub daemon start` prints `STARTED pid=<n>` or `ALREADY RUNNING pid=<n>` and removes `run/daemon.stopped`; `hub daemon stop` writes `run/daemon.stopped` first, kills only when `identify_daemon` is `Ours` and the image is killable, prints `STOPPED`; `hub daemon run` runs in the console (`ALREADY RUNNING` + exit 1 when held). Texts copied from `commands.ts`.
  - `hub daemon start` spawns, hidden and detached, `hubw.exe daemon run` on Windows (sibling of `hub.exe`; falls back to `hub.exe` with `CREATE_NO_WINDOW`) or `hub daemon run` on Unix (`setsid`, stdio null).
  - `--version`/`-v` prints `hub <CARGO_PKG_VERSION>`; `--help`/`-h`/`help` prints the usage block (commands not in this slice are omitted, not stubbed).
  - `hubw` (Windows only, `#![windows_subsystem = "windows"]`) calls the same `run`; on other OSes the bin is not built (`required-features` or `cfg` gate).

- [ ] **Step 1: Write the failing tests.** Each spawns `env!("CARGO_BIN_EXE_hub")` with `HOME`/`USERPROFILE`/`LOCALAPPDATA`/`XDG_DATA_HOME` pointed at a temp dir (the env `resolve_config` already reads) and `PATH` at a dir holding the fake claude: `status_stopped_exit_3`; `start_then_status_running_then_stop`; `stop_writes_stopped_file_first`; `stop_refuses_foreign_pid` (lock naming a live non-hub process, e.g. the test's own `sleep`, is left alone and reported `STOPPED` without a kill); `run_twice_second_reports_already_running`; `version_and_help`; `unknown_command_exit_2`.
- [ ] **Step 2: Run** `cargo test -p hub --test cli` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** → PASS on macOS; push; Windows job green.
- [ ] **Step 5: Commit** `git commit -m "feat(rust): hub daemon run|start|stop|status and hubw on Windows"`.

### Task 15: Read-only TUI

**Files:**
- Create: `crates/hub-tui/{Cargo.toml,src/lib.rs,src/app.rs,src/list.rs,src/status_pane.rs,src/history_pane.rs,src/format.rs}`, `crates/hub-tui/tests/{format,render}.rs`
- Modify: `crates/hub/src/cli.rs` (no args → start daemon if not running, then `hub_tui::run`)
- Port from: `src/tui/{App,SessionList,StatusPane,HistoryPane}.tsx`, `src/tui/{format,keys}.ts`; tests from `test/tui/status-lines.test.ts`

**Interfaces:**
- Consumes: `Store::open_read_only`, `read_snapshot`, `Store::{history, search}`, `Platform::focus_session_window`, `clean_text`.
- Produces: `pub fn run(store_path: &Path, platform: Arc<dyn Platform>, known_profiles: &[&str]) -> io::Result<()>`; `format::{ago(ms: i64) -> String, hhmm(ts: &str) -> String, session_label(name, session_id) -> String, state_label(state, ended_at) -> String, profile_header(&str) -> String, role_label(role, from_name) -> String, wrap(text, width, indent) -> Vec<String>}`; `status_pane::status_lines(view: &SessionView, meta: &SnapshotMeta, now: i64) -> Vec<String>`.
- Behaviour from the TS panes: list 30% wide grouped by profile (`.claude`, `.claude-personal`, others alphabetical), name or first 8 chars of the id, state label (busy, idle, "needs you", ended), cwd line; panes `1` Status, `2` History (pages of 50, PgUp older, PgDn, End newest), `/` search (Enter, Esc); `↑/↓`, `r`, `j` (toast 3 s: focused title, the reason, or "not supported on this OS yet"), `q`; refresh every 1 s. Key `3` shows one line: `Sending arrives in the next version of the Rust hub.` Every printed store string goes through `clean_text` (already applied by `read_snapshot`; history/search rows here).

- [ ] **Step 1: Write the failing tests:** port `test/tui/status-lines.test.ts` (same view, same expected lines); `format` unit tests for each function with the TS outputs (copy from the TS tests or compute through the goldens script's `clean` area if needed); `render_smoke` with `ratatui::backend::TestBackend` 100×30 over a seeded store: list shows both profile headers, the selected session's Status pane shows `Q1` and `answered`, pressing `2` shows a history line, `/` + `deploy` + Enter shows a `[deploy]` snippet.
- [ ] **Step 2: Run** `cargo test -p hub-tui` → FAIL.
- [ ] **Step 3: Implement.** App state is a plain struct updated by a `handle_key(KeyEvent) -> Action` function (unit-testable without a terminal); the terminal loop polls crossterm events with a 1 s timeout for refresh; the terminal is restored on panic (`std::panic::set_hook` wrapper).
- [ ] **Step 4: Run** → PASS. Manual check on macOS: `cargo run -p hub` with `HOME` set to a temp dir containing a copy of `test/fixtures/records/*.jsonl` arranged as `<tmp>/.claude/projects/p/<uuid>.jsonl` and `PATH` pointing at the fake claude in `ok` mode; the TUI lists the session and shows its history.
- [ ] **Step 5: Commit** `git commit -m "feat(rust): read-only TUI (sessions, status, history, search)"`.

### Task 16: Slice wrap-up and PR

**Files:**
- Create: `crates/README.md` (how to build, test and run the Rust hub during the transition; which TS parts it replaces so far; the manual-testing rule: stop the TS daemon first with `hub daemon stop`)

- [ ] **Step 1: Full verification:** `cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings && cargo test --locked --workspace && scripts/coverage-gate.sh && cargo deny --locked check` → all pass locally; `bun test` still passes (TS untouched).
- [ ] **Step 2: Push and wait for CI** on the fork: `rust-check`, `rust-windows`, `rust-macos` and the existing `ci.yml` jobs green (`gh run watch`).
- [ ] **Step 3: Open the PR** from `DFanso:feat/rust-core` to `ravindu0823/session-hub:main`, title `Rust rewrite, slice 1: core, daemon and read-only TUI (#8)`. Body: what the slice contains, what it does not (send, tray, install), how parity is proven (goldens, ported tests), the approved "Ask before" items (crates table link, `rust.yml`, `ci.yml` semgrep + goldens step), how to try it, `Refs #8`. No AI attribution.
- [ ] **Step 4: Report** the PR link and CI state to the coordinator with `bridge_send`.
