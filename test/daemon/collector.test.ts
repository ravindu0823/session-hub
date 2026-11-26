import { Database } from "bun:sqlite";
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import {
  appendFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  statSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { RETENTION_MS, SAFETY_POLL_MS } from "../../src/config.ts";
import { runDaemon } from "../../src/daemon/collector.ts";
import { acquireLock } from "../../src/daemon/lock.ts";
import type { Logger } from "../../src/daemon/log.ts";
import { type ClaudeCli, realClaudeCli } from "../../src/discovery/agents.ts";
import type { Classified } from "../../src/ingest/records.ts";
import { FakePlatform } from "../../src/platform/fake.ts";
import { Store } from "../../src/store/db.ts";

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** realClaudeCli with a call counter and an optional gate that holds calls in flight. */
function gatedCli(): {
  cli: ClaudeCli;
  calls: () => number;
  hold: () => void;
  release: () => void;
} {
  const real = realClaudeCli(FAKE);
  let n = 0;
  let gate: Promise<void> | null = null;
  let open = () => {};
  return {
    cli: {
      agents: async (dir) => {
        n += 1;
        if (gate) await gate;
        return real.agents(dir);
      },
    },
    calls: () => n,
    hold: () => {
      gate = new Promise((r) => {
        open = r;
      });
    },
    release: () => {
      gate = null;
      open();
    },
  };
}

const FAKE = ["bun", join(import.meta.dir, "..", "fakes", "fake-claude.ts")];
const S1 = "11111111-2222-4333-8444-555555555555";
const R1 = "99999999-2222-4333-8444-555555555555";
const E1 = "eeeeeeee-2222-4333-8444-555555555555";
const O1 = "00000000-2222-4333-8444-555555555555";
const SECRET = `ghp_${"q".repeat(36)}`;
const TEXT = {
  prompt: "fixture owner prompt alpha",
  phase: "fixture phase bravo",
  remaining: "fixture remaining charlie",
  waiting: "fixture waiting delta",
  partial: "fixture partial echo",
  plain: "fixture plain foxtrot",
  ended: "fixture ended golf",
};
const STATUS_REPLY = [
  `**Progress: about 40% overall. Phase: ${TEXT.phase}.**`,
  "",
  "Body.",
  "",
  "### What's remaining",
  `1. ${TEXT.remaining} with ${SECRET}`,
  "",
  "### Waiting on you",
  `- **New:** Q1: ${TEXT.waiting} token ${SECRET}`,
].join("\n");

let root: string;
let profileDir: string;
let projDir: string;
let dataDir: string;
let relayDir: string;
let agentsJson: string;
let store: Store;
let clock: number;
const now = () => clock;
let events: { level: string; event: string; fields: Record<string, unknown> }[];
let watchHandlers: (() => void)[];
let stops: (() => Promise<void>)[];

const capture: Logger = {
  info: (event, fields = {}) => events.push({ level: "info", event, fields }),
  warn: (event, fields = {}) => events.push({ level: "warn", event, fields }),
  error: (event, fields = {}) => events.push({ level: "error", event, fields }),
};

/** Mirrors watchSessions: a missing sessions folder gives onError and a no-op disposer. */
function fakeWatch(dir: string, onChange: () => void, onError?: (e: Error) => void): () => void {
  if (!existsSync(join(dir, "sessions"))) {
    onError?.(new Error("sessions folder missing"));
    return () => {};
  }
  watchHandlers.push(onChange);
  return () => {
    watchHandlers = watchHandlers.filter((h) => h !== onChange);
  };
}
const fire = () => {
  for (const h of watchHandlers) h();
};

beforeEach(() => {
  root = mkdtempSync(join(tmpdir(), "hub-daemon-"));
  profileDir = join(root, "profile");
  projDir = join(profileDir, "projects", "C--fixture");
  mkdirSync(join(profileDir, "sessions"), { recursive: true });
  mkdirSync(projDir, { recursive: true });
  dataDir = join(root, "data");
  relayDir = join(dataDir, "relay");
  mkdirSync(relayDir, { recursive: true });
  agentsJson = join(root, "agents.json");
  writeFileSync(agentsJson, "[]");
  process.env.FAKE_AGENTS_JSON = agentsJson;
  delete process.env.FAKE_AGENTS_EXIT;
  store = Store.open(join(dataDir, "hub.db"));
  clock = Date.now();
  events = [];
  watchHandlers = [];
  stops = [];
});

afterEach(async () => {
  for (const s of stops) await s();
  store.close();
  delete process.env.FAKE_AGENTS_JSON;
  delete process.env.FAKE_AGENTS_EXIT;
  rmSync(root, { recursive: true, force: true });
});

function setAgents(entries: Record<string, unknown>[]): void {
  writeFileSync(agentsJson, JSON.stringify(entries));
}

function agent(sessionId: string, over: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    pid: 4242,
    cwd: "C:\\fixture",
    kind: "interactive",
    startedAt: 1_000,
    sessionId,
    name: `name-${sessionId}`,
    status: "idle",
    ...over,
  };
}

let seq = 0;
function ts(): string {
  seq += 1;
  // Relative to the test clock: a fixed date drifts out of the 7-day window and gets purged.
  return new Date(clock - 3_600_000 + seq * 1_000).toISOString();
}
/** `at`: the record's own time (epoch ms); default a fixed fixture time. */
function userLine(uuid: string, text: string, at?: number): string {
  return JSON.stringify({
    type: "user",
    isSidechain: false,
    uuid,
    timestamp: at === undefined ? ts() : new Date(at).toISOString(),
    cwd: "C:\\fixture-cwd",
    origin: { kind: "human" },
    message: { role: "user", content: text },
  });
}
function assistantLine(uuid: string, text: string, at?: number): string {
  return JSON.stringify({
    type: "assistant",
    isSidechain: false,
    uuid,
    timestamp: at === undefined ? ts() : new Date(at).toISOString(),
    message: { role: "assistant", content: [{ type: "text", text }] },
  });
}
function modeLine(mode: string): string {
  return JSON.stringify({ type: "permission-mode", permissionMode: mode, sessionId: "x" });
}
/** A parsed record type whose shape changed: raises the format banner. */
function unknownLine(): string {
  return JSON.stringify({ type: "user", uuid: "z", version: "9.9.9", message: "s" });
}
/** A record type this build does not know: diagnostics only. */
function unrecognizedLine(): string {
  return JSON.stringify({ type: "brand-new-record-type", uuid: "y", version: "9.9.8" });
}
function writeTranscript(sessionId: string, lines: string[], dir = projDir): string {
  const p = join(dir, `${sessionId}.jsonl`);
  writeFileSync(p, lines.map((l) => `${l}\n`).join(""));
  return p;
}

type Over = Partial<Parameters<typeof runDaemon>[0]>;

function start(over: Over = {}): { done: Promise<void>; stop: () => Promise<void> } {
  const ac = new AbortController();
  const done = runDaemon({
    cli: realClaudeCli(FAKE),
    store,
    platform: new FakePlatform(),
    now,
    profiles: [{ id: "claude", dir: profileDir }],
    dataDir,
    relayDir,
    signal: ac.signal,
    watch: fakeWatch,
    log: capture,
    ...over,
  });
  done.catch(() => {});
  let stopped = false;
  const stop = async () => {
    if (stopped) return;
    stopped = true;
    ac.abort();
    await done.catch(() => {});
  };
  stops.push(stop);
  return { done, stop };
}

async function waitFor(cond: () => boolean, ms = 4_000): Promise<void> {
  const end = Date.now() + ms;
  while (!cond()) {
    if (Date.now() > end) throw new Error("waitFor timed out");
    await new Promise((r) => setTimeout(r, 25));
  }
}
const passes = () => events.filter((e) => e.event === "pass").length;
async function nextPass(): Promise<void> {
  const n = passes();
  fire();
  await waitFor(() => passes() > n);
}
const session = (id: string) => store.listSessions().find((s) => s.sessionId === id);
/** Waits for the condition and for the startup pass to have finished. */
const settled = (cond: () => boolean = () => true) => waitFor(() => cond() && passes() >= 1);

describe("runDaemon", () => {
  test("new live session appears with parsed status, mode, masked messages", async () => {
    writeTranscript(S1, [
      modeLine("default"),
      userLine("u1", TEXT.prompt),
      modeLine("bypassPermissions"),
      assistantLine("a1", STATUS_REPLY),
    ]);
    setAgents([agent(S1, { status: "busy" })]);
    const got: [string, Classified[]][] = [];
    start({ onRecords: (id, recs) => got.push([id, recs]) });
    await waitFor(() => session(S1)?.status != null);

    const s = session(S1);
    expect(s?.name).toBe(`name-${S1}`);
    expect(s?.state).toBe("busy");
    expect(s?.profile).toBe("claude");
    expect(s?.endedAt).toBeNull();
    expect(s?.permissionMode).toBe("bypassPermissions");
    expect(s?.transcriptPath).toBe(join(projDir, `${S1}.jsonl`));
    expect(s?.status?.progressPct).toBe(40);
    expect(s?.status?.phase).toBe(TEXT.phase);
    expect(s?.status?.replyUuid).toBe("a1");
    expect(s?.latestReply).toMatchObject({ replyUuid: "a1", hasStatus: true });
    expect(s?.status?.waiting[0]?.id).toBe("Q1");
    expect(s?.status?.waiting[0]?.isNew).toBe(true);

    const hist = store.history(S1, 10);
    expect(hist.map((m) => m.role)).toEqual(["owner", "assistant"]);
    expect(hist[0]?.text).toBe(TEXT.prompt);
    expect(got.some(([id, recs]) => id === S1 && recs.some((r) => r.kind === "owner-prompt"))).toBe(
      true,
    );
  });

  test("a secret in a status reply is masked in stored status and messages", async () => {
    writeTranscript(S1, [assistantLine("a1", STATUS_REPLY)]);
    setAgents([agent(S1)]);
    start();
    await waitFor(() => session(S1)?.status != null);
    const st = session(S1)?.status;
    const all = JSON.stringify(st) + JSON.stringify(store.history(S1, 10));
    expect(all).not.toContain(SECRET);
    expect(all).toContain("[masked:github]");
    expect(st?.remaining[0]).toContain("[masked:github]");
    expect(st?.waiting[0]?.text).toContain("[masked:github]");
  });

  test("session missing from agents is marked ended; reappearing clears it", async () => {
    setAgents([agent(S1)]);
    start();
    await settled(() => session(S1) !== undefined);
    clock += 5_000;
    const endedAt = clock;
    setAgents([]);
    await nextPass();
    expect(session(S1)?.endedAt).toBe(endedAt);
    setAgents([agent(S1)]);
    await nextPass();
    expect(session(S1)?.endedAt).toBeNull();
  });

  test("identical agents output leaves the row and the database alone; a change bumps updatedAt", async () => {
    writeTranscript(S1, [userLine("u-1", TEXT.prompt)]);
    setAgents([agent(S1)]);
    start();
    await settled(() => session(S1) !== undefined);
    await nextPass(); // the transcript path is found and stored on a pass; this one has settled
    const reader = Store.open(join(dataDir, "hub.db"));
    try {
      const before = session(S1)?.updatedAt;
      const version = reader.dataVersion();
      clock += 60_000;
      await nextPass();
      clock += 60_000;
      await nextPass();
      expect(session(S1)?.updatedAt).toBe(before as number);
      expect(reader.dataVersion()).toBe(version);
      setAgents([agent(S1, { status: "busy" })]);
      await nextPass();
      expect(session(S1)?.state).toBe("busy");
      expect(session(S1)?.updatedAt).toBe(clock);
      expect(reader.dataVersion()).not.toBe(version);
    } finally {
      reader.close();
    }
  });

  test("a new reply bumps updatedAt", async () => {
    const path = writeTranscript(S1, [userLine("u-1", TEXT.prompt)]);
    setAgents([agent(S1)]);
    start();
    await settled(() => session(S1) !== undefined);
    await nextPass();
    const before = session(S1)?.updatedAt as number;
    clock += 60_000;
    appendFileSync(path, `${assistantLine("a-1", STATUS_REPLY, clock)}\n`);
    await nextPass();
    expect(session(S1)?.updatedAt).toBe(clock);
    expect(session(S1)?.updatedAt).toBeGreaterThan(before);
  });

  test("an appended owner message moves updatedAt", async () => {
    const path = writeTranscript(S1, [userLine("u-1", TEXT.prompt)]);
    setAgents([agent(S1)]);
    start();
    await settled(() => session(S1) !== undefined);
    await nextPass();
    const before = session(S1)?.updatedAt as number;
    clock += 60_000;
    appendFileSync(path, `${userLine("u-2", TEXT.plain, clock)}\n`);
    await nextPass();
    expect(session(S1)?.updatedAt).toBe(clock);
    expect(session(S1)?.updatedAt).toBeGreaterThan(before);
  });

  test("own relay sessions are hidden", async () => {
    setAgents([agent(R1, { name: "session-hub-relay", cwd: relayDir }), agent(S1)]);
    start();
    await waitFor(() => session(S1) !== undefined);
    expect(session(R1)).toBeUndefined();
  });

  test("partial line written mid-record then completed: one message, no duplicate", async () => {
    const path = writeTranscript(S1, [userLine("u1", TEXT.prompt)]);
    setAgents([agent(S1)]);
    start();
    await settled(() => store.history(S1, 10).length === 1);
    const line = assistantLine("a1", TEXT.partial);
    const cut = Math.floor(line.length / 2);
    appendFileSync(path, line.slice(0, cut));
    await nextPass();
    expect(store.history(S1, 10).length).toBe(1);
    appendFileSync(path, `${line.slice(cut)}\n`);
    await nextPass();
    await nextPass();
    const hist = store.history(S1, 10);
    expect(hist.map((m) => m.uuid)).toEqual(["u1", "a1"]);
    expect(hist[1]?.text).toBe(TEXT.partial);
    expect(session(S1)?.latestReply).toMatchObject({ replyUuid: "a1", hasStatus: false });
  });

  test("transcript created after the session starts is found on a later pass", async () => {
    setAgents([agent(S1)]);
    start();
    await settled(() => session(S1) !== undefined);
    expect(session(S1)?.transcriptPath).toBe("");
    writeTranscript(S1, [userLine("u1", TEXT.prompt)]);
    await nextPass();
    await waitFor(() => store.history(S1, 10).length === 1);
    expect(session(S1)?.transcriptPath).toBe(join(projDir, `${S1}.jsonl`));
  });

  test("unknown records are counted in meta", async () => {
    writeTranscript(S1, [
      unknownLine(),
      unrecognizedLine(),
      unknownLine(),
      unrecognizedLine(),
      unrecognizedLine(),
      userLine("u1", TEXT.prompt),
    ]);
    setAgents([agent(S1)]);
    start();
    await settled(() => store.history(S1, 10).length === 1);
    expect(store.getMeta("unknown_count")).toBe("2");
    expect(store.getMeta("unknown_version")).toBe("9.9.9");
    expect(store.getMeta("unrecognized_count")).toBe("3");
    expect(store.getMeta("unrecognized_types")).toBe('["brand-new-record-type"]');
    const pass = events.find((e) => e.event === "pass" && e.fields.lines === 6);
    expect(pass?.fields).toMatchObject({ unknown: 2, unrecognized: 3 });
  });

  test("agents failure sets stale_since, backs off 2 then 4 s, clears on recovery", async () => {
    const calls: number[] = [];
    const real = realClaudeCli(FAKE);
    const cli: ClaudeCli = {
      agents: (dir) => {
        calls.push(clock);
        return real.agents(dir);
      },
    };
    setAgents([agent(S1)]);
    process.env.FAKE_AGENTS_EXIT = "1";
    const t0 = clock;
    start({ cli });
    const failures = () => events.filter((e) => e.event === "agents_failed").length;
    await waitFor(() => failures() === 1 && passes() >= 1);
    expect(calls).toEqual([t0]);
    expect(store.getMeta("stale_since:claude")).toBe(String(t0));

    await nextPass();
    clock += 1_999;
    await nextPass();
    expect(calls.length).toBe(1);
    clock += 1;
    fire();
    await waitFor(() => failures() === 2);

    clock += 3_999;
    await nextPass();
    expect(calls.length).toBe(2);
    clock += 1;
    fire();
    await waitFor(() => failures() === 3);
    expect(calls).toEqual([t0, t0 + 2_000, t0 + 6_000]);
    expect(store.getMeta("stale_since:claude")).toBe(String(t0));

    delete process.env.FAKE_AGENTS_EXIT;
    clock += 8_000;
    fire();
    await waitFor(() => store.getMeta("stale_since:claude") === null);
    expect(calls).toEqual([t0, t0 + 2_000, t0 + 6_000, t0 + 14_000]);
    expect(session(S1)).toBeDefined();
  });

  test("after a restart with agents failing, stored live sessions are still tailed", async () => {
    const path = writeTranscript(S1, [userLine("u1", TEXT.prompt)]);
    store.upsertSession({
      sessionId: S1,
      profile: "claude",
      name: "n",
      cwd: "C:\\fixture",
      pid: 1,
      kind: "interactive",
      status: "idle",
      startedAt: 1,
      updatedAt: 1,
      endedAt: null,
      transcriptPath: path,
      permissionMode: null,
    });
    process.env.FAKE_AGENTS_EXIT = "1";
    start();
    await settled(() => store.history(S1, 10).length === 1);
    expect(session(S1)?.endedAt).toBeNull();
    expect(store.getMeta("stale_since:claude")).not.toBeNull();
  });

  test("agents failure keeps the profile's sessions as they are", async () => {
    setAgents([agent(S1)]);
    start();
    await settled(() => session(S1) !== undefined);
    process.env.FAKE_AGENTS_EXIT = "1";
    setAgents([]);
    await nextPass();
    expect(session(S1)?.endedAt).toBeNull();
    expect(store.getMeta("stale_since:claude")).not.toBeNull();
  });

  test("second daemon is refused by the lock", async () => {
    start();
    await waitFor(() => passes() >= 1);
    const ac = new AbortController();
    await expect(
      runDaemon({
        cli: realClaudeCli(FAKE),
        store,
        platform: new FakePlatform(),
        now,
        profiles: [],
        dataDir,
        relayDir,
        signal: ac.signal,
        watch: fakeWatch,
        log: capture,
      }),
    ).rejects.toThrow("daemon already running");
  });

  test("a stale lock left by a dead daemon is taken over", async () => {
    const runDir = join(dataDir, "run");
    mkdirSync(runDir, { recursive: true });
    const p = Bun.spawn(["bun", "-e", "0"], {
      stdin: "ignore",
      stdout: "ignore",
      stderr: "ignore",
    });
    await p.exited;
    writeFileSync(join(runDir, "daemon.lock"), String(p.pid));
    start();
    await waitFor(() => passes() >= 1);
    expect(readFileSync(join(runDir, "daemon.lock"), "utf8")).toBe(String(process.pid));
  });

  test("data dir that is not private: throws before taking the lock", async () => {
    const ac = new AbortController();
    const platform = new FakePlatform({ private: false });
    await expect(
      runDaemon({
        cli: realClaudeCli(FAKE),
        store,
        platform,
        now,
        profiles: [],
        dataDir,
        relayDir,
        signal: ac.signal,
        watch: fakeWatch,
        log: capture,
      }),
    ).rejects.toThrow("data dir is not private");
    expect(platform.calls.map((c) => c.method)).toEqual(["isPrivateDir"]);
    expect(existsSync(join(dataDir, "run", "daemon.lock"))).toBe(false);
  });

  test("abort resolves runDaemon and releases the lock", async () => {
    setAgents([agent(S1)]);
    const { done, stop } = start();
    await waitFor(() => passes() >= 1);
    expect(existsSync(join(dataDir, "run", "daemon.lock"))).toBe(true);
    await stop();
    await done;
    expect(existsSync(join(dataDir, "run", "daemon.lock"))).toBe(false);
    const lock = acquireLock(join(dataDir, "run"));
    expect(lock).not.toBeNull();
    lock?.release();
  });

  test("recent transcript indexed at startup; one older than 7 days or not uuid-named is not", async () => {
    const recent = writeTranscript(E1, [
      modeLine("default"),
      userLine("eu1", TEXT.ended),
      assistantLine("ea1", STATUS_REPLY),
    ]);
    const old = writeTranscript(O1, [userLine("ou1", TEXT.plain)]);
    const oldTime = new Date(clock - RETENTION_MS - 86_400_000);
    utimesSync(old, oldTime, oldTime);
    mkdirSync(join(projDir, "nested"));
    writeTranscript("deep", [userLine("du1", TEXT.plain)], join(projDir, "nested"));
    writeFileSync(join(projDir, "notes.txt"), "x");
    writeTranscript("not-a-session-id", [userLine("nu1", TEXT.plain)]);

    start();
    await waitFor(() => session(E1)?.status != null);
    const e = session(E1);
    expect(e?.kind).toBe("ended");
    expect(e?.state).toBe("unknown");
    expect(e?.name).toBe("");
    expect(e?.cwd).toBe("C:\\fixture-cwd");
    expect(e?.transcriptPath).toBe(recent);
    expect(e?.endedAt).toBe(Math.floor(statSync(recent).mtimeMs));
    expect(store.history(E1, 10).map((m) => m.uuid)).toEqual(["eu1", "ea1"]);
    expect(session(O1)).toBeUndefined();
    expect(session("not-a-session-id")).toBeUndefined();
    expect(session("deep")).toBeUndefined();
  });

  test("default logger writes to <dataDir>/run/daemon.log with no message text", async () => {
    writeTranscript(S1, [
      userLine("u1", TEXT.prompt),
      assistantLine("a1", STATUS_REPLY),
      assistantLine("a2", TEXT.plain),
    ]);
    writeTranscript(E1, [userLine("eu1", TEXT.ended)]);
    setAgents([agent(S1)]);
    const { stop } = start({ log: undefined });
    await waitFor(() => store.history(S1, 10).length === 3 && store.history(E1, 5).length === 1);
    process.env.FAKE_AGENTS_EXIT = "1";
    fire();
    const logFile = join(dataDir, "run", "daemon.log");
    await waitFor(
      () => existsSync(logFile) && readFileSync(logFile, "utf8").includes("agents_failed"),
    );
    await stop();
    const runFiles = readdirSync(join(dataDir, "run")).filter((f) => f.startsWith("daemon.log"));
    const content = runFiles.map((f) => readFileSync(join(dataDir, "run", f), "utf8")).join("\n");
    expect(content.length).toBeGreaterThan(0);
    for (const s of [...Object.values(TEXT), SECRET, "Progress", "Waiting on you"]) {
      expect(content).not.toContain(s);
    }
  });

  test("invalid session ids from agents are skipped and only counted", async () => {
    setAgents([agent("....evil"), agent(S1)]);
    start();
    await settled(() => session(S1) !== undefined);
    expect(store.listSessions().map((s) => s.sessionId)).toEqual([S1]);
    const ev = events.find((e) => e.event === "invalid_session_ids");
    expect(ev?.fields).toEqual({ profile: "claude", count: 1 });
  });

  test("a transcript cut by the 8 MB budget is fully indexed after a restart", async () => {
    const pad = "x".repeat(1000);
    const total = 17_000;
    const lines: string[] = [];
    for (let i = 0; i < total; i++) {
      lines.push(
        JSON.stringify({
          type: "user",
          isSidechain: false,
          uuid: `big-${i}`,
          timestamp: new Date(clock - 3_600_000).toISOString(),
          origin: { kind: "human" },
          message: { role: "user", content: `p${i}` },
          pad,
        }),
      );
    }
    const path = writeTranscript(S1, lines);
    const size = statSync(path).size;
    expect(size).toBeGreaterThan(16 * 1024 * 1024);
    setAgents([agent(S1)]);
    const first = start();
    await settled(() => store.history(S1, 1).length === 1);
    await first.stop();
    const afterFirst = store.history(S1, total).length;
    expect(afterFirst).toBeLessThan(total);
    expect(store.getTail(path)?.offset).toBeLessThan(size);

    start();
    await waitFor(() => store.history(S1, total).length === total, 15_000);
    expect(store.getTail(path)?.offset).toBe(size);
  }, 30_000);

  test("triggers during a pass give exactly one follow-up pass", async () => {
    const g = gatedCli();
    setAgents([agent(S1)]);
    start({ cli: g.cli });
    await settled(() => session(S1) !== undefined);
    const p0 = passes();
    const c0 = g.calls();
    g.hold();
    fire();
    await waitFor(() => g.calls() === c0 + 1);
    fire();
    fire();
    fire();
    g.release();
    await waitFor(() => passes() === p0 + 2);
    await sleep(500);
    expect(passes()).toBe(p0 + 2);
    expect(g.calls()).toBe(c0 + 2);
  });

  test("abort during an in-flight failing agents call re-arms no retry timer", async () => {
    const g = gatedCli();
    const { stop } = start({ cli: g.cli });
    await settled();
    const c0 = g.calls();
    process.env.FAKE_AGENTS_EXIT = "1";
    g.hold();
    fire();
    await waitFor(() => g.calls() === c0 + 1);
    const stopping = stop();
    g.release();
    await stopping;
    expect(events.some((e) => e.event === "agents_failed")).toBe(false);
    expect(store.getMeta("stale_since:claude")).toBeNull();
    await sleep(2_300);
    expect(g.calls()).toBe(c0 + 1);
  });

  test("a missing sessions folder is watched once it exists", async () => {
    rmSync(join(profileDir, "sessions"), { recursive: true });
    setAgents([agent(S1)]);
    start();
    await settled(() => session(S1) !== undefined);
    expect(watchHandlers.length).toBe(0);
    expect(events.filter((e) => e.event === "watch_error").length).toBe(1);
    mkdirSync(join(profileDir, "sessions"));
    await waitFor(() => watchHandlers.length === 1, 5_000);
    expect(events.some((e) => e.event === "watch_rearmed")).toBe(true);
    await nextPass();
  }, 10_000);

  test("a deleted live transcript clears the cached path and is found again", async () => {
    const path = writeTranscript(S1, [userLine("u1", TEXT.prompt)]);
    setAgents([agent(S1)]);
    start();
    await settled(() => store.history(S1, 10).length === 1);
    rmSync(path);
    await nextPass();
    await nextPass();
    expect(session(S1)?.transcriptPath).toBe("");
    expect(events.filter((e) => e.event === "transcript_missing").length).toBe(1);
    const otherProj = join(profileDir, "projects", "D--moved");
    mkdirSync(otherProj);
    const moved = writeTranscript(
      S1,
      [userLine("u1", TEXT.prompt), userLine("u2", TEXT.plain)],
      otherProj,
    );
    await nextPass();
    await waitFor(() => store.history(S1, 10).length === 2);
    expect(session(S1)?.transcriptPath).toBe(moved);
  });

  test("purge hourly and vacuum daily; SQLITE_BUSY is retried on the next tick", async () => {
    const other = new Database(join(dataDir, "hub.db"));
    other.run("BEGIN IMMEDIATE");
    start();
    await waitFor(() => events.some((e) => e.event === "purge_failed"), 9_000);
    const failed = events.find((e) => e.event === "purge_failed");
    expect(failed?.fields.busy).toBe(true);
    expect(store.getMeta("last_purge")).toBeNull();
    expect(store.getMeta("last_vacuum")).toBeNull();
    other.run("COMMIT");
    other.close();

    const count = (ev: string) => events.filter((e) => e.event === ev).length;
    await nextPass();
    await waitFor(() => count("purge") === 1 && count("vacuum") === 1);
    expect(store.getMeta("last_purge")).toBe(String(clock));
    expect(store.getMeta("last_vacuum")).toBe(String(clock));

    clock += 3_599_999;
    await nextPass();
    expect(count("purge")).toBe(1);
    clock += 1;
    await nextPass();
    expect(count("purge")).toBe(2);
    expect(count("vacuum")).toBe(1);
    clock += 86_400_000;
    await nextPass();
    expect(count("purge")).toBe(3);
    expect(count("vacuum")).toBe(2);
  }, 20_000);

  test("a purge failing with a non-busy error is retried at most every 5 minutes", async () => {
    const failing = Object.create(store) as Store;
    failing.purge = () => {
      throw new Error("disk I/O error");
    };
    start({ store: failing });
    await settled();
    const count = (ev: string) => events.filter((e) => e.event === ev).length;
    expect(count("purge_failed")).toBe(1);
    expect(events.find((e) => e.event === "purge_failed")?.fields.busy).toBe(false);
    expect(count("vacuum")).toBe(1);
    await nextPass();
    await nextPass();
    expect(count("purge_failed")).toBe(1);
    clock += 299_999;
    await nextPass();
    expect(count("purge_failed")).toBe(1);
    clock += 1;
    await nextPass();
    expect(count("purge_failed")).toBe(2);
  });

  test("a watcher that keeps dying logs once until a re-armed one survives a safety poll", async () => {
    let arms = 0;
    let dying = true;
    let kill: (() => void) | null = null;
    const dyingWatch = (
      _dir: string,
      onChange: () => void,
      onError?: (e: Error) => void,
    ): (() => void) => {
      arms += 1;
      watchHandlers.push(onChange);
      kill = () => onError?.(new Error("watcher died"));
      if (dying) setTimeout(() => kill?.(), 10);
      return () => {
        watchHandlers = watchHandlers.filter((h) => h !== onChange);
      };
    };
    const count = (ev: string) => events.filter((e) => e.event === ev).length;
    start({ watch: dyingWatch });
    await settled();
    await waitFor(() => arms >= 4, 9_000);
    expect(count("watch_error")).toBe(1);
    expect(count("watch_rearmed")).toBe(1);

    dying = false;
    const a = arms;
    await waitFor(() => arms === a + 1, 3_000);
    clock += SAFETY_POLL_MS;
    await sleep(2_300); // one tick sees the watcher stable for a safety poll
    expect(count("watch_error")).toBe(1);
    (kill as (() => void) | null)?.();
    await waitFor(() => count("watch_error") === 2 && count("watch_rearmed") === 2, 3_000);
  }, 25_000);

  test("a last line that never completes is not re-read while the file is unchanged", async () => {
    const path = writeTranscript(S1, [userLine("u1", TEXT.prompt)]);
    const line = userLine("u2", TEXT.partial);
    const cut = Math.floor(line.length / 2);
    appendFileSync(path, line.slice(0, cut));
    setAgents([agent(S1)]);
    start();
    await settled(() => store.history(S1, 10).length === 1);
    await nextPass();
    await nextPass();
    expect(events.filter((e) => e.event === "pass").at(-1)?.fields.files).toBe(0);
    appendFileSync(path, `${line.slice(cut)}\n`);
    await nextPass();
    await waitFor(() => store.history(S1, 10).length === 2);
  });
});
