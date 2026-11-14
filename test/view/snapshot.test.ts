import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { type SessionRow, Store } from "../../src/store/db";
import { cleanText } from "../../src/tui/sanitize";
import { readSnapshot } from "../../src/view/snapshot";

let dir: string;
let store: Store;
const OPTS = { daemonRunning: true, knownProfiles: ["claude", "claude-personal"] } as const;

function session(over: Partial<SessionRow>): SessionRow {
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

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "hub-snap-"));
  store = Store.open(join(dir, "index.db"));
});

afterEach(() => {
  store.close();
  rmSync(dir, { recursive: true, force: true });
});

describe("readSnapshot", () => {
  test("totals count items and approvals", () => {
    store.upsertSession(session({ sessionId: "A", name: "a" }));
    store.setStatus(
      "A",
      {
        progressPct: 10,
        phase: "p",
        remaining: [],
        waiting: [
          { id: "Q1", text: "one", isNew: false, children: [] },
          { id: "Q2", text: "two", isNew: false, children: [] },
        ],
        recommendations: [],
      },
      "r1",
      "t1",
    );
    store.upsertSession(session({ sessionId: "B", name: "b", status: "waiting" }));
    store.upsertSession(session({ sessionId: "C", name: "c", endedAt: 5000 }));
    const snap = readSnapshot(store, OPTS);
    expect(snap.totals).toEqual({ waitingItems: 2, needsApproval: 1, live: 2 });
    expect(snap.sessions).toHaveLength(3);
  });

  test("cleans strings", () => {
    store.upsertSession(session({ name: "\x1b[31mX" }));
    const snap = readSnapshot(store, OPTS);
    expect(snap.sessions[0]?.name).toBe(cleanText("\x1b[31mX"));
    expect(snap.sessions[0]?.name).not.toContain("\x1b");
  });

  test("meta.daemon follows the flag", () => {
    expect(readSnapshot(store, { ...OPTS, daemonRunning: false }).meta.daemon).toBe("stopped");
    expect(readSnapshot(store, OPTS).meta.daemon).toBe("running");
  });

  test("meta carries unknown count, version and stale profiles", () => {
    store.setMeta("unknown_count", "3");
    store.setMeta("unknown_version", "9.9");
    store.setMeta("stale_since:claude", "1234");
    const { meta } = readSnapshot(store, OPTS);
    expect(meta.unknownCount).toBe(3);
    expect(meta.unknownVersion).toBe("9.9");
    expect(meta.stale).toEqual([{ profile: "claude", since: 1234 }]);
  });

  test("meta carries the stray relay count for 24 h after the last stray, else 0", () => {
    const now = 1_000_000_000_000;
    const at = (t: number) => readSnapshot(store, { ...OPTS, now: t }).meta.relayStrayCount;
    expect(at(now)).toBe(0);
    store.setMeta("relay_stray_count", "2");
    // counted by an older build that kept no time: not shown
    expect(at(now)).toBe(0);
    store.setMeta("relay_stray_last", String(now - 1000));
    expect(at(now)).toBe(2);
    expect(at(now - 1000 + 24 * 3600_000)).toBe(2);
    expect(at(now - 1000 + 24 * 3600_000 + 1)).toBe(0);
    store.setMeta("relay_stray_count", "x");
    expect(at(now)).toBe(0);
  });
});
