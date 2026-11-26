import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { HEARTBEAT_MS, MAX_LINE_BYTES, POLL_MS } from "../../src/bridge/protocol";
import {
  type BridgeDeps,
  type BridgeIo,
  RESTART_STABLE_MS,
  runBridge,
  SEND_TRACK_MS,
} from "../../src/bridge/server";
import { DELIVERY_WINDOW_MS, EXPIRE_EVERY_MS } from "../../src/config";
import { SendRefused, type SendRequest } from "../../src/send/relay";
import { type SendRow, type SessionRow, Store } from "../../src/store/db";
import { FakeClock, FakeIo, flush, type Out } from "./harness";

let dir: string;
let dbPath: string;
/** The bridge's own connection. */
let store: Store;
/** Stands in for the daemon: commits through this connection change data_version for the bridge. */
let other: Store;
let io: FakeIo;
let clock: FakeClock;

function session(over: Partial<SessionRow> = {}): SessionRow {
  return {
    sessionId: "s1",
    profile: "claude",
    name: "alpha",
    cwd: "C:/x",
    pid: 100,
    kind: "interactive",
    status: "idle",
    startedAt: 1000,
    updatedAt: 2000,
    endedAt: null,
    transcriptPath: "C:/x/s1.jsonl",
    permissionMode: null,
    ...over,
  };
}

/** Inserts a pending send the way send() does, through the bridge's connection. */
function insertSend(sessionId: string, at = 5000): SendRow {
  const r: SendRow = {
    sessionId,
    profile: "claude",
    name: "alpha",
    sha256: "0".repeat(64),
    createdAt: at,
    state: "pending",
    detail: null,
  };
  return store.tx(() => ({ ...r, id: store.createSend(r) }));
}

interface Ctl {
  done: Promise<number>;
  daemon: boolean;
  startCalls: number;
  sendCalls: number;
}

function start(
  over: Partial<BridgeDeps> = {},
  opts: { daemon?: boolean; stop?: Promise<number> } = {},
): Ctl {
  const ctl = { daemon: opts.daemon ?? true, startCalls: 0, sendCalls: 0 } as Ctl;
  const deps: BridgeDeps = {
    store: () => store,
    daemonRunning: () => ctl.daemon,
    startDaemon: async () => {
      ctl.startCalls += 1;
      ctl.daemon = true;
      return true;
    },
    send: async (req, onCreated) => {
      ctl.sendCalls += 1;
      const row = insertSend(req.sessionId);
      onCreated(row);
      return row;
    },
    now: clock.now,
    sleep: clock.sleep,
    version: "1.2.3",
    ...over,
  };
  const bridgeIo: BridgeIo = { lines: io.lines, write: (line) => io.write(line) };
  if (opts.stop !== undefined) bridgeIo.stop = opts.stop;
  ctl.done = runBridge(bridgeIo, deps);
  return ctl;
}

const snaps = (): Out[] => io.of("snapshot");
const replies = (): Out[] => io.of("reply");
const sendEvents = (): Out[] => io.of("send");
const errors = (): Out[] => io.of("error");
type Snap = { sessions: { name: string }[]; meta: { daemon: string }; totals: unknown };
const snapOf = (o: Out | undefined): Snap => (o as unknown as { snapshot: Snap }).snapshot;

async function request(line: string): Promise<void> {
  io.push(line);
  await flush();
  await flush();
}

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "hub-bridge-"));
  dbPath = join(dir, "hub.db");
  other = Store.open(dbPath);
  other.upsertSession(session());
  store = Store.open(dbPath);
  io = new FakeIo();
  clock = new FakeClock(1_000_000);
});

afterEach(() => {
  io.end();
  store.close();
  other.close();
  rmSync(dir, { recursive: true, force: true });
});

describe("snapshots", () => {
  test("hello first, then one snapshot", async () => {
    start();
    await flush();
    const out = io.out;
    expect(out).toHaveLength(2);
    expect(out[0]).toEqual({ type: "hello", protocol: 1, version: "1.2.3" });
    expect(out[1]?.type).toBe("snapshot");
    expect(out[1]?.seq).toBe(1);
    const snap = snapOf(out[1]);
    expect(snap.sessions.map((s) => s.name)).toEqual(["alpha"]);
    expect(snap.meta.daemon).toBe("running");
    expect(snap.totals).toEqual({ waitingItems: 0, needsApproval: 0, live: 1 });
  });

  test("no snapshot when data_version unchanged across 10 polls", async () => {
    start();
    await clock.advance(10 * 250);
    expect(snaps()).toHaveLength(1);
  });

  test("snapshot after another connection commits", async () => {
    start();
    await flush();
    other.upsertSession(session({ name: "beta" }));
    await clock.advance(POLL_MS);
    expect(snaps()).toHaveLength(2);
    expect(snaps()[1]?.seq).toBe(2);
    expect(snapOf(snaps()[1]).sessions[0]?.name).toBe("beta");
  });

  test("identical data after a commit emits nothing (hash equal)", async () => {
    start();
    await flush();
    other.upsertSession(session()); // same row again: data_version moves, the snapshot does not
    other.setMeta("unrelated", "x");
    await clock.advance(1000);
    expect(snaps()).toHaveLength(1);
  });

  test("change storm: at most 4 snapshots in a second, the last shows the final state", async () => {
    start();
    await flush();
    for (let i = 0; i < 50; i++) {
      other.upsertSession(session({ name: `v${i}` }));
      await clock.advance(20);
    }
    const inStorm = snaps().length - 1;
    expect(inStorm).toBeGreaterThan(0);
    expect(inStorm).toBeLessThanOrEqual(4);
    await clock.advance(500);
    expect(snapOf(snaps().at(-1)).sessions[0]?.name).toBe("v49");
  });

  test("15 s of no change: a small heartbeat line with the current seq, no snapshot", async () => {
    start();
    await clock.advance(HEARTBEAT_MS - POLL_MS);
    expect(io.of("heartbeat")).toHaveLength(0);
    await clock.advance(POLL_MS);
    expect(snaps()).toHaveLength(1);
    expect(io.of("heartbeat")).toEqual([{ type: "heartbeat", seq: 1 }]);
    expect(io.raw.at(-1)).toBe('{"type":"heartbeat","seq":1}');
    await clock.advance(HEARTBEAT_MS - POLL_MS);
    expect(io.of("heartbeat")).toHaveLength(1);
    await clock.advance(POLL_MS);
    expect(io.of("heartbeat")).toHaveLength(2);
    // A change sends a snapshot again (seq 2); the next heartbeat names it.
    other.upsertSession(session({ name: "beta" }));
    await clock.advance(POLL_MS);
    expect(snaps().map((s) => s.seq)).toEqual([1, 2]);
    await clock.advance(HEARTBEAT_MS);
    expect(io.of("heartbeat").at(-1)).toEqual({ type: "heartbeat", seq: 2 });
    expect(snaps()).toHaveLength(2);
  });

  test("a commit that leaves the snapshot unchanged still gets heartbeats, not snapshots", async () => {
    start();
    await flush();
    other.setMeta("unrelated", "x");
    await clock.advance(HEARTBEAT_MS);
    expect(snaps()).toHaveLength(1);
    expect(io.of("heartbeat")).toHaveLength(1);
  });

  test("the snapshot never carries a transcript path", async () => {
    start();
    await flush();
    const s = snaps()[0]?.snapshot as { sessions: Record<string, unknown>[] };
    expect(s.sessions[0]?.sessionId).toBe("s1");
    expect("transcriptPath" in (s.sessions[0] ?? {})).toBe(false);
    expect(io.raw.join(" ")).not.toContain("s1.jsonl");
  });

  test("over 1 MiB: a reduced snapshot without the status of ended sessions, no error", async () => {
    start();
    await flush();
    other.upsertSession(session({ sessionId: "s2", name: "old", endedAt: 3000 }));
    const huge = Array.from({ length: 20 }, (_, i) => `${i}${"r".repeat(60_000)}`);
    other.setStatus(
      "s2",
      { progressPct: 1, phase: null, remaining: huge, waiting: [], recommendations: [] },
      "r1",
      "t1",
    );
    other.setStatus(
      "s1",
      { progressPct: 5, phase: "live", remaining: [], waiting: [], recommendations: [] },
      "r2",
      "t2",
    );
    await clock.advance(POLL_MS);
    expect(errors()).toEqual([]);
    expect(snaps()).toHaveLength(2);
    expect(Buffer.byteLength(io.raw.at(-1) ?? "")).toBeLessThanOrEqual(MAX_LINE_BYTES);
    const s = snaps()[1]?.snapshot as {
      sessions: { sessionId: string; status: { phase: string | null } | null }[];
    };
    const byId = new Map(s.sessions.map((x) => [x.sessionId, x]));
    expect(byId.get("s2")?.status).toBeNull();
    expect(byId.get("s1")?.status?.phase).toBe("live");
  });

  test("an oversized snapshot reports one error and is retried only at the next change", async () => {
    start();
    await flush();
    const huge = Array.from({ length: 20 }, (_, i) => `${i}${"r".repeat(60_000)}`);
    other.setStatus(
      "s1",
      { progressPct: 1, phase: null, remaining: huge, waiting: [], recommendations: [] },
      "r1",
      "t1",
    );
    await clock.advance(POLL_MS);
    expect(errors()).toEqual([
      { type: "error", code: "too-large", message: "the snapshot does not fit in one line" },
    ]);
    await clock.advance(45_000);
    expect(errors()).toHaveLength(1);
    expect(snaps()).toHaveLength(1);
    // Heartbeats keep the link alive meanwhile (the tray's watchdog ends a bridge silent for 45 s).
    expect(io.of("heartbeat").length).toBeGreaterThanOrEqual(2);
    other.setStatus(
      "s1",
      { progressPct: 2, phase: null, remaining: ["small"], waiting: [], recommendations: [] },
      "r2",
      "t2",
    );
    await clock.advance(POLL_MS);
    expect(snaps()).toHaveLength(2);
    expect(snaps()[1]?.seq).toBe(2);
    expect(errors()).toHaveLength(1);
  });

  test("a daemon that stops or starts changes meta.daemon", async () => {
    const ctl = start();
    await flush();
    ctl.daemon = false;
    await clock.advance(POLL_MS);
    expect(snapOf(snaps().at(-1)).meta.daemon).toBe("stopped");
    ctl.daemon = true;
    await clock.advance(POLL_MS);
    expect(snapOf(snaps().at(-1)).meta.daemon).toBe("running");
  });
});

describe("daemon start", () => {
  test("starts the daemon once when it is not running", async () => {
    let running = false;
    let calls = 0;
    start({
      daemonRunning: () => running,
      startDaemon: async () => {
        calls += 1;
        running = true;
        return true;
      },
    });
    await flush();
    expect(calls).toBe(1);
    expect(snapOf(snaps()[0]).meta.daemon).toBe("stopped");
    await clock.advance(POLL_MS);
    expect(snapOf(snaps().at(-1)).meta.daemon).toBe("running");
    await clock.advance(5000);
    expect(calls).toBe(1);
  });

  test("a daemon that dies is started again (RES-L1)", async () => {
    const ctl = start();
    await flush();
    expect(ctl.startCalls).toBe(0);
    ctl.daemon = false; // crashed
    await clock.advance(POLL_MS);
    await flush();
    expect(ctl.startCalls).toBe(1);
    expect(ctl.daemon).toBe(true);
  });

  test("a daemon the owner stopped (hub daemon stop) is left stopped", async () => {
    const ctl = start({ daemonStopRequested: () => true });
    await flush();
    ctl.daemon = false;
    await clock.advance(5 * POLL_MS);
    expect(ctl.startCalls).toBe(0);
  });

  test("failed restarts back off (5 s, 30 s, 2 min), then stop with one error", async () => {
    let running = true;
    let calls = 0;
    start({
      daemonRunning: () => running,
      startDaemon: async () => {
        calls += 1;
        return false;
      },
    });
    await flush();
    running = false;
    await clock.advance(POLL_MS);
    await flush();
    expect(calls).toBe(1);
    await clock.advance(4_000);
    expect(calls).toBe(1);
    await clock.advance(1_500);
    await flush();
    expect(calls).toBe(2);
    await clock.advance(30_500);
    await flush();
    expect(calls).toBe(3);
    await clock.advance(120_500);
    await flush();
    expect(calls).toBe(4);
    await clock.advance(600_000);
    expect(calls).toBe(4);
    expect(errors().filter((e) => e.code === "daemon-restart-failed")).toHaveLength(1);
  });

  test("a daemon that starts but keeps dying is not restarted forever; one that stays up resets", async () => {
    let running = true;
    let calls = 0;
    start({
      daemonRunning: () => running,
      startDaemon: async () => {
        calls += 1;
        running = true;
        return true;
      },
    });
    await flush();
    const crash = async (ms: number) => {
      running = false;
      await clock.advance(ms);
      await flush();
    };
    await crash(POLL_MS); // immediate restart
    expect(calls).toBe(1);
    await clock.advance(1_000); // up for a second, then dies again
    await crash(POLL_MS);
    expect(calls).toBe(1); // waits 5 s now
    await clock.advance(5_500);
    await flush();
    expect(calls).toBe(2);
    await crash(31_000);
    expect(calls).toBe(3);
    await crash(121_000);
    expect(calls).toBe(4);
    await crash(600_000);
    expect(calls).toBe(4);
    expect(errors().filter((e) => e.code === "daemon-restart-failed")).toHaveLength(1);
  });

  test("after 2 min of uptime a later crash restarts at once again", async () => {
    let running = true;
    let calls = 0;
    start({
      daemonRunning: () => running,
      startDaemon: async () => {
        calls += 1;
        running = true;
        return true;
      },
    });
    await flush();
    running = false;
    await clock.advance(POLL_MS);
    await flush();
    expect(calls).toBe(1);
    await clock.advance(RESTART_STABLE_MS + POLL_MS);
    running = false;
    await clock.advance(POLL_MS);
    await flush();
    expect(calls).toBe(2);
  });

  test("does not start a running daemon; startDaemon request replies with the result", async () => {
    const ctl = start();
    await flush();
    expect(ctl.startCalls).toBe(0);
    await request('{"id":3,"type":"startDaemon"}');
    expect(replies()).toEqual([{ type: "reply", id: 3, ok: true, result: { running: true } }]);
    expect(ctl.startCalls).toBe(1);
  });
});

describe("history and search", () => {
  test("history/search reply with masked rows", async () => {
    other.addMessages("s1", [
      {
        uuid: "m1",
        sessionId: "s1",
        ts: "2026-11-13T10:00:00.000Z",
        role: "owner",
        fromName: null,
        text: "deploy note password=hunter22 done \u001b[31mred\u001b[0m",
      },
      {
        uuid: "m2",
        sessionId: "s1",
        ts: "2026-11-13T10:01:00.000Z",
        role: "assistant",
        fromName: null,
        text: "second",
      },
    ]);
    start();
    await flush();
    await request('{"id":1,"type":"history","sessionId":"s1","limit":10}');
    const h = replies()[0] as unknown as {
      ok: boolean;
      result: { messages: { uuid: string; text: string }[]; truncated: boolean; more: boolean };
    };
    expect(h.ok).toBe(true);
    expect(h.result.truncated).toBe(false);
    expect(h.result.more).toBe(false);
    expect(h.result.messages[0]).not.toHaveProperty("textTruncated");
    expect(h.result.messages.map((m) => m.uuid)).toEqual(["m1", "m2"]);
    const text = h.result.messages[0]?.text ?? "";
    expect(text).not.toContain("hunter22");
    expect(text).toContain("[masked:");
    expect(text).not.toContain("\u001b");

    await request(
      '{"id":2,"type":"history","sessionId":"s1","limit":10,"beforeTs":"2026-11-13T10:01:00.000Z"}',
    );
    const older = replies()[1] as unknown as {
      result: { messages: { uuid: string }[]; more: boolean };
    };
    expect(older.result.messages.map((m) => m.uuid)).toEqual(["m1"]);
    expect(older.result.more).toBe(false);

    await request('{"id":3,"type":"search","q":"deploy","limit":5}');
    await request('{"id":4,"type":"history","sessionId":"s1","limit":1}');
    const page = replies()[3] as unknown as {
      result: { messages: { uuid: string }[]; more: boolean; truncated: boolean };
    };
    expect(page.result.messages.map((m) => m.uuid)).toEqual(["m2"]);
    expect(page.result.more).toBe(true);
    expect(page.result.truncated).toBe(false);
    const s = replies()[2] as unknown as {
      ok: boolean;
      result: { hits: { uuid: string; snippet: string }[] };
    };
    expect(s.ok).toBe(true);
    expect(s.result.hits.map((x) => x.uuid)).toEqual(["m1"]);
    expect(s.result.hits[0]?.snippet).not.toContain("hunter22");
  });

  test("history pages with beforeTs and beforeUuid keep rows that share a ts", async () => {
    const row = (uuid: string, ts: string) => ({
      uuid,
      sessionId: "s1",
      ts,
      role: "assistant" as const,
      fromName: null,
      text: uuid,
    });
    other.addMessages("s1", [
      row("k1", "2026-11-13T10:01:00.000Z"),
      row("k2", "2026-11-13T10:01:00.000Z"),
      row("k3", "2026-11-13T10:02:00.000Z"),
    ]);
    start();
    await flush();
    await request('{"id":1,"type":"history","sessionId":"s1","limit":2}');
    await request(
      '{"id":2,"type":"history","sessionId":"s1","limit":2,"beforeTs":"2026-11-13T10:01:00.000Z","beforeUuid":"k2"}',
    );
    const pages = replies() as unknown as {
      result: { messages: { uuid: string }[]; more: boolean };
    }[];
    expect(pages[0]?.result.messages.map((m) => m.uuid)).toEqual(["k2", "k3"]);
    expect(pages[0]?.result.more).toBe(true);
    expect(pages[1]?.result.messages.map((m) => m.uuid)).toEqual(["k1"]);
    expect(pages[1]?.result.more).toBe(false);
  });

  test("a history page too big for one line is shortened, keeping the newest", async () => {
    const big = "y".repeat(100_000);
    other.addMessages(
      "s1",
      Array.from({ length: 30 }, (_, i) => ({
        uuid: `b${i}`,
        sessionId: "s1",
        ts: `2026-11-13T10:${String(i).padStart(2, "0")}:00.000Z`,
        role: "assistant" as const,
        fromName: null,
        text: big,
      })),
    );
    start();
    await flush();
    await request('{"id":1,"type":"history","sessionId":"s1","limit":30}');
    const line = io.raw.find((l) => l.includes('"type":"reply"')) ?? "";
    expect(Buffer.byteLength(line)).toBeLessThanOrEqual(MAX_LINE_BYTES);
    const r = JSON.parse(line) as {
      ok: boolean;
      result: { messages: { uuid: string }[]; truncated: boolean; more: boolean };
    };
    expect(r.ok).toBe(true);
    expect(r.result.messages.length).toBeGreaterThan(0);
    expect(r.result.messages.length).toBeLessThan(30);
    expect(r.result.messages.at(-1)?.uuid).toBe("b29");
    expect(r.result.truncated).toBe(true);
    expect(r.result.more).toBe(true);
  });

  test("a message over 256 KiB comes back cut to 256 KiB with textTruncated", async () => {
    // 3-byte characters: the cut must land on a character boundary.
    other.addMessages("s1", [
      {
        uuid: "huge",
        sessionId: "s1",
        ts: "2026-11-13T10:00:00.000Z",
        role: "assistant",
        fromName: null,
        text: "\u20ac".repeat(400_000),
      },
      {
        uuid: "small",
        sessionId: "s1",
        ts: "2026-11-13T10:01:00.000Z",
        role: "owner",
        fromName: null,
        text: "ok",
      },
    ]);
    start();
    await flush();
    await request('{"id":1,"type":"history","sessionId":"s1","limit":10}');
    const r = replies()[0] as unknown as {
      ok: boolean;
      result: {
        messages: { uuid: string; text: string; textTruncated?: boolean }[];
        truncated: boolean;
      };
    };
    expect(r.ok).toBe(true);
    expect(r.result.truncated).toBe(false);
    const [huge, small] = r.result.messages;
    expect(huge?.uuid).toBe("huge");
    expect(huge?.textTruncated).toBe(true);
    const bytes = Buffer.byteLength(huge?.text ?? "");
    expect(bytes).toBeLessThanOrEqual(256 * 1024);
    expect(bytes).toBeGreaterThan(256 * 1024 - 3);
    expect(huge?.text).not.toContain("\ufffd");
    expect(small?.textTruncated).toBeUndefined();
  });
});

describe("send", () => {
  test("looks-secret is refused with a plain message; allowSecret is passed through (LLM-03)", async () => {
    const seen: SendRequest[] = [];
    start({
      send: async (req, onCreated) => {
        seen.push(req);
        if (!req.allowSecret) throw new SendRefused("looks-secret");
        const row = insertSend(req.sessionId);
        onCreated(row);
        return row;
      },
    });
    await flush();
    await request('{"id":8,"type":"send","sessionId":"s1","text":"k"}');
    expect(replies()[0]).toMatchObject({ id: 8, ok: false, error: { code: "looks-secret" } });
    expect(
      String((replies()[0] as unknown as { error: { message: string } }).error.message),
    ).toContain("Send anyway?");
    await request('{"id":9,"type":"send","sessionId":"s1","text":"k","allowSecret":true}');
    expect(replies()[1]).toMatchObject({ id: 9, ok: true });
    expect(seen.map((r) => r.allowSecret)).toEqual([undefined, true]);
  });

  test("send replies on onCreated, then emits send events", async () => {
    let finishRelay: () => void = () => {};
    let created: SendRow | null = null;
    start({
      send: async (req, onCreated) => {
        const row = insertSend(req.sessionId);
        created = row;
        onCreated(row);
        await new Promise<void>((r) => {
          finishRelay = r;
        });
        return store.getSend(row.id as number) as SendRow;
      },
    });
    await flush();
    await request('{"id":7,"type":"send","sessionId":"s1","text":"hello"}');
    const id = (created as SendRow | null)?.id as number;
    expect(replies()).toEqual([
      { type: "reply", id: 7, ok: true, result: { sendId: id, state: "pending" } },
    ]);
    expect(sendEvents()).toHaveLength(0);

    other.transitionSend(id, "held", "recipient busy");
    await clock.advance(POLL_MS);
    other.transitionSend(id, "delivered", null);
    await clock.advance(POLL_MS);
    expect(sendEvents()).toEqual([
      {
        type: "send",
        send: { id, sessionId: "s1", state: "held", detail: "recipient busy" },
      },
      { type: "send", send: { id, sessionId: "s1", state: "delivered", detail: null } },
    ]);

    finishRelay();
    await flush();
    other.setMeta("poke", "1");
    await clock.advance(500);
    expect(sendEvents()).toHaveLength(2);
  });

  test("a relay failure written by the bridge itself is emitted when send returns", async () => {
    start({
      send: async (req, onCreated) => {
        const row = insertSend(req.sessionId);
        onCreated(row);
        await flush();
        store.transitionSend(row.id as number, "failed", "exit 1");
        return store.getSend(row.id as number) as SendRow;
      },
    });
    await flush();
    await request('{"id":1,"type":"send","sessionId":"s1","text":"x"}');
    await flush();
    expect(sendEvents().map((e) => (e.send as { state: string }).state)).toEqual(["failed"]);
  });

  test("tracking covers the delivery window plus two expiry ticks", () => {
    expect(SEND_TRACK_MS).toBe(DELIVERY_WINDOW_MS + 2 * EXPIRE_EVERY_MS);
  });

  test("the daemon's expired, written on its tick after the 30 min window, reaches the tray", async () => {
    start();
    await flush();
    await request('{"id":1,"type":"send","sessionId":"s1","text":"x"}');
    const id = Number((replies()[0]?.result as { sendId?: number } | undefined)?.sendId);
    clock.t += DELIVERY_WINDOW_MS + EXPIRE_EVERY_MS;
    other.transitionSend(id, "expired", "no delivery within 30 minutes");
    await clock.advance(POLL_MS);
    expect(sendEvents().map((e) => (e.send as { state: string }).state)).toEqual(["expired"]);
  });

  test("a send is no longer watched once the track limit has passed", async () => {
    start();
    await flush();
    await request('{"id":1,"type":"send","sessionId":"s1","text":"x"}');
    const id = Number((replies()[0]?.result as { sendId?: number } | undefined)?.sendId);
    clock.t += SEND_TRACK_MS + 1;
    other.setMeta("poke", "1"); // a pass past the limit reads the row once, then drops it
    await clock.advance(POLL_MS);
    other.transitionSend(id, "held", "late");
    await clock.advance(POLL_MS);
    expect(sendEvents()).toHaveLength(0);
  });

  test("failed is not final: a later delivery is still emitted", async () => {
    start();
    await flush();
    await request('{"id":1,"type":"send","sessionId":"s1","text":"x"}');
    const id = Number((replies()[0]?.result as { sendId?: number } | undefined)?.sendId);
    other.transitionSend(id, "failed", "exit 1");
    await clock.advance(POLL_MS);
    other.transitionSend(id, "delivered");
    await clock.advance(POLL_MS);
    expect(sendEvents().map((e) => (e.send as { state: string }).state)).toEqual([
      "failed",
      "delivered",
    ]);
  });

  test("send-in-flight reply carries sendId", async () => {
    const open = insertSend("s1");
    start({
      send: async () => {
        throw new SendRefused("send-in-flight");
      },
    });
    await flush();
    await request('{"id":4,"type":"send","sessionId":"s1","text":"x"}');
    expect(replies()).toEqual([
      {
        type: "reply",
        id: 4,
        ok: false,
        error: { code: "send-in-flight", message: "send refused: send-in-flight", sendId: open.id },
      },
    ]);
  });

  test("a store whose openSendFor throws still yields one reply and the bridge keeps serving", async () => {
    store.openSendFor = () => {
      throw new Error("disk I/O error");
    };
    start({
      send: async () => {
        throw new SendRefused("send-in-flight");
      },
    });
    await flush();
    await request('{"id":4,"type":"send","sessionId":"s1","text":"x"}');
    await request('{"id":5,"type":"ping"}');
    expect(replies()).toEqual([
      {
        type: "reply",
        id: 4,
        ok: false,
        error: { code: "send-in-flight", message: "send refused: send-in-flight" },
      },
      { type: "reply", id: 5, ok: true, result: {} },
    ]);
  });

  test("an unexpected failure inside a request handler replies internal", async () => {
    const write = io.write;
    let failed = false;
    io.write = (line: string) => {
      if (!failed && line.includes('"id":3')) {
        failed = true;
        throw new Error("write failed");
      }
      write(line);
    };
    start();
    await flush();
    await request('{"id":3,"type":"startDaemon"}');
    await request('{"id":4,"type":"ping"}');
    expect(replies()).toEqual([
      { type: "reply", id: 3, ok: false, error: { code: "internal", message: "write failed" } },
      { type: "reply", id: 4, ok: true, result: {} },
    ]);
  });

  test("other refusals reply with the reason as the code", async () => {
    start({
      send: async () => {
        throw new SendRefused("session-not-live");
      },
    });
    await flush();
    await request('{"id":5,"type":"send","sessionId":"s1","text":"x"}');
    expect(replies()).toEqual([
      {
        type: "reply",
        id: 5,
        ok: false,
        error: { code: "session-not-live", message: "send refused: session-not-live" },
      },
    ]);
  });

  test("an unexpected send error before the row exists replies send-error", async () => {
    start({
      send: async () => {
        throw new Error("claude agents failed");
      },
    });
    await flush();
    await request('{"id":6,"type":"send","sessionId":"s1","text":"x"}');
    expect(replies()).toEqual([
      {
        type: "reply",
        id: 6,
        ok: false,
        error: { code: "send-error", message: "claude agents failed" },
      },
    ]);
  });
});

describe("db missing", () => {
  test("empty snapshot until the db appears, then a real one; requests reply no-store", async () => {
    let current: Store | null = null;
    const ctl = start({ store: () => current }, { daemon: false });
    await flush();
    const first = snapOf(snaps()[0]);
    expect(first.meta.daemon).toBe("stopped");
    expect(first.sessions).toEqual([]);
    expect(first.totals).toEqual({ waitingItems: 0, needsApproval: 0, live: 0 });
    await request('{"id":1,"type":"history","sessionId":"s1","limit":5}');
    await request('{"id":2,"type":"send","sessionId":"s1","text":"x"}');
    expect(replies().map((r) => (r.error as { code: string }).code)).toEqual([
      "no-store",
      "no-store",
    ]);
    expect(ctl.sendCalls).toBe(0);
    await clock.advance(1000);
    expect(await Promise.race([ctl.done, Promise.resolve("running")])).toBe("running");

    current = store;
    await clock.advance(POLL_MS);
    expect(snapOf(snaps().at(-1)).sessions.map((s) => s.name)).toEqual(["alpha"]);
  });

  test("a store that fails to open reports one error and retries", async () => {
    let fail = true;
    start({
      store: () => {
        if (fail) throw new Error("database is locked");
        return store;
      },
    });
    await flush();
    expect(snapOf(snaps()[0]).sessions).toEqual([]);
    await clock.advance(2000);
    expect(errors()).toEqual([
      { type: "error", code: "store-error", message: "database is locked" },
    ]);
    fail = false;
    await clock.advance(POLL_MS);
    expect(snapOf(snaps().at(-1)).sessions).toHaveLength(1);
  });
});

describe("lifecycle and input", () => {
  test("stdin end exits 0", async () => {
    const ctl = start();
    await flush();
    io.end();
    expect(await ctl.done).toBe(0);
  });

  test("a broken output stops the bridge with the code it gives", async () => {
    let broken: (code: number) => void = () => {};
    const stop = new Promise<number>((r) => {
      broken = r;
    });
    const ctl = start({}, { stop });
    await flush();
    broken(1);
    expect(await ctl.done).toBe(1);
    const before = io.raw.length;
    await request('{"id":1,"type":"ping"}');
    other.upsertSession(session({ name: "after" }));
    await clock.advance(1000);
    expect(io.raw.length).toBe(before);
  });

  test("shutdown exits 0", async () => {
    const ctl = start();
    await flush();
    io.push('{"type":"shutdown"}');
    expect(await ctl.done).toBe(0);
    const before = io.raw.length;
    other.upsertSession(session({ name: "after" }));
    await clock.advance(1000);
    expect(io.raw.length).toBe(before);
  });

  test("malformed line is counted and skipped, next request still answered", async () => {
    start();
    await flush();
    await request("{not json");
    await request('{"id":5,"type":"ping"}');
    await request("x".repeat(MAX_LINE_BYTES + 1));
    await request('{"type":"focus","sessionId":"s1"}');
    await request('{"id":6,"type":"focus","sessionId":"s1"}');
    await request("");
    expect(errors().map((e) => e.code)).toEqual(["bad-json", "too-long", "unknown-type"]);
    expect(String(errors()[2]?.message)).toContain("3 malformed");
    expect(replies()).toEqual([
      { type: "reply", id: 5, ok: true, result: {} },
      {
        type: "reply",
        id: 6,
        ok: false,
        error: { code: "unknown-type", message: "unknown request type" },
      },
    ]);
  });
});

describe("answer", () => {
  function ask(role = "web"): void {
    other.mail().createDecision({
      role,
      question: "Ship?",
      options: ["yes", "no"],
      recommendation: "yes",
      fromSession: null,
      createdAt: 1,
    });
  }

  test("stores the owner's answer, mails it to the role and sends a new snapshot", async () => {
    ask();
    start();
    await flush();
    const before = snaps().length;
    await request('{"id":3,"type":"answer","decisionId":"D-0001","text":"yes"}');
    expect(replies()[0]).toMatchObject({
      id: 3,
      ok: true,
      result: { result: "answered", role: "web" },
    });
    const d = other.mail().getDecision("D-0001");
    expect(d?.answer).toBe("yes");
    expect(d?.answeredVia).toBe("tray");
    expect(other.mail().inbox("web")[0]?.body).toBe(
      "D-0001 answered: yes (recorded via tray, outside Claude Code; not proof of who sent it)",
    );
    await clock.advance(POLL_MS);
    expect(snaps().length).toBeGreaterThan(before);
    const last = snaps().at(-1) as unknown as {
      snapshot: { mail: { decisions: { id: string; state: string }[] } };
    };
    expect(last.snapshot.mail.decisions[0]).toMatchObject({ id: "D-0001", state: "answered" });
  });

  test("a different second answer replies conflict and changes nothing", async () => {
    ask();
    other.mail().answerDecision("D-0001", "yes", "cli", 2);
    start();
    await flush();
    await request('{"id":4,"type":"answer","decisionId":"D-0001","text":"no"}');
    expect(replies()[0]).toMatchObject({ id: 4, ok: true, result: { result: "conflict" } });
    expect(other.mail().getDecision("D-0001")?.answer).toBe("yes");
    await request('{"id":5,"type":"answer","decisionId":"D-0001","text":"no","replace":true}');
    expect(replies()[1]).toMatchObject({ id: 5, ok: true, result: { result: "replaced" } });
  });

  test("a refused answer replies with the refusal's reason", async () => {
    ask();
    start();
    await flush();
    await request('{"id":6,"type":"answer","decisionId":"D-0001","text":"  "}');
    expect(replies()[0]).toMatchObject({ id: 6, ok: false, error: { code: "empty-body" } });
  });

  test("bad answer requests are bad-request", async () => {
    start();
    await flush();
    await request('{"id":7,"type":"answer","decisionId":"D-0001"}');
    await request('{"id":8,"type":"answer","decisionId":"D-0001","text":"y","replace":"x"}');
    expect(replies().map((r) => (r as { error?: { code: string } }).error?.code)).toEqual([
      "bad-request",
      "bad-request",
    ]);
  });

  test("before the index exists the answer is refused with no-store", async () => {
    start({ store: () => null });
    await flush();
    await request('{"id":9,"type":"answer","decisionId":"D-0001","text":"yes"}');
    expect(replies()[0]).toMatchObject({ id: 9, ok: false, error: { code: "no-store" } });
  });

  test("the snapshot uses the roles file the bridge reads", async () => {
    other.upsertSession(session({ sessionId: "L", name: "web-2" }));
    other.mail().insert({
      thread: "T",
      fromRole: "api",
      fromSession: null,
      toRole: "web",
      kind: "task",
      body: "b",
      evidence: null,
      createdAt: 1,
      expiresAt: 9e12,
    });
    start({
      roles: () => ({
        roles: [{ role: "web", profile: null, sessions: ["web-*"] }],
        expiryMs: 1,
        ackTimeoutMs: 1,
      }),
    });
    await flush();
    const s = snaps().at(-1) as unknown as { snapshot: { mail: { waitingRoles: unknown[] } } };
    expect(s.snapshot.mail.waitingRoles).toEqual([]);
  });
});
