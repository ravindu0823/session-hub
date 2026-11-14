import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { appendFileSync, mkdtempSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  type IngestCtx,
  ingestFile,
  MAX_BYTES_PER_PASS,
  MIN_READ_BYTES,
} from "../../src/daemon/ingest-pass.ts";
import { type SessionRow, Store } from "../../src/store/db.ts";

const SID = "11111111-2222-4333-8444-555555555555";
let dir: string;
let store: Store;
let path: string;
const row: SessionRow = {
  sessionId: SID,
  profile: "claude",
  name: "n",
  cwd: "C:\\fixture",
  pid: 1,
  kind: "interactive",
  status: "idle",
  startedAt: 1,
  updatedAt: 1,
  endedAt: null,
  transcriptPath: "",
  permissionMode: null,
};

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "hub-ingest-"));
  store = Store.open(join(dir, "hub.db"));
  path = join(dir, `${SID}.jsonl`);
  store.upsertSession({ ...row, transcriptPath: path });
});
afterEach(() => {
  store.close();
  rmSync(dir, { recursive: true, force: true });
});

function ctx(): IngestCtx {
  return { store, getRow: () => undefined, rowChanged: () => {}, now: () => 1_000 };
}
const noErr = (e: unknown) => {
  throw e;
};

function bigLine(i: number): string {
  return JSON.stringify({
    type: "user",
    isSidechain: false,
    uuid: `b-${i}`,
    timestamp: "2026-11-13T08:00:00.000Z",
    origin: { kind: "human" },
    message: { role: "user", content: `p${i}` },
    pad: "x".repeat(1000),
  });
}

describe("updatedAt follows the messages' own time", () => {
  const NOW = Date.parse("2026-11-13T12:00:00.000Z");
  function line(uuid: string, ts: string): string {
    return `${JSON.stringify({
      type: "user",
      isSidechain: false,
      uuid,
      timestamp: ts,
      origin: { kind: "human" },
      message: { role: "user", content: uuid },
    })}\n`;
  }
  function run(updatedAt: number, lines: string[]): Promise<SessionRow | null> {
    let seen: SessionRow | null = null;
    const cached = { ...row, transcriptPath: path, updatedAt };
    writeFileSync(path, lines.join(""));
    return ingestFile(
      {
        store,
        getRow: () => cached,
        rowChanged: (r) => {
          seen = r;
        },
        now: () => NOW,
      },
      SID,
      path,
      noErr,
    ).then(() => seen);
  }

  test("a new message dates the session by its timestamp, not by now", async () => {
    const r = await run(NOW - 3_600_000, [line("m1", "2026-11-13T11:30:00.000Z")]);
    expect(r?.updatedAt).toBe(Date.parse("2026-11-13T11:30:00.000Z"));
  });

  test("a replayed batch of older messages leaves updatedAt alone (no row write)", async () => {
    const r = await run(NOW - 60_000, [
      line("m1", "2026-11-12T08:00:00.000Z"),
      line("m2", "2026-11-12T09:00:00.000Z"),
    ]);
    expect(r).toBeNull();
    expect(store.history(SID, 10)).toHaveLength(2);
  });

  test("a timestamp ahead of our clock is capped at now", async () => {
    const r = await run(NOW - 60_000, [line("m1", "2027-01-01T00:00:00.000Z")]);
    expect(r?.updatedAt).toBe(NOW);
  });

  test("the newest of the batch counts, in any order", async () => {
    const r = await run(1, [
      line("m1", "2026-11-13T10:00:00.000Z"),
      line("m2", "2026-11-13T11:00:00.000Z"),
      line("m3", "2026-11-13T09:00:00.000Z"),
    ]);
    expect(r?.updatedAt).toBe(Date.parse("2026-11-13T11:00:00.000Z"));
  });
});

describe("ingestFile", () => {
  test("one call reads at most the 8 MB budget (plus the 64 KiB floor and one line), the next finishes", async () => {
    const total = 12_500;
    writeFileSync(path, Array.from({ length: total }, (_, i) => `${bigLine(i)}\n`).join(""));
    const size = statSync(path).size;
    const lineLen = bigLine(0).length + 1;
    expect(size).toBeGreaterThan(12 * 1024 * 1024);

    const r1 = await ingestFile(ctx(), SID, path, noErr);
    const off1 = store.getTail(path)?.offset ?? 0;
    expect(r1.more).toBe(true);
    expect(off1).toBeLessThanOrEqual(MAX_BYTES_PER_PASS + MIN_READ_BYTES + lineLen);
    expect(off1).toBeGreaterThan(MAX_BYTES_PER_PASS - lineLen);

    const r2 = await ingestFile(ctx(), SID, path, noErr);
    expect(r2.more).toBe(false);
    expect(store.getTail(path)?.offset).toBe(size);
    expect(r1.lines + r2.lines).toBe(total);
    expect(store.history(SID, total + 1).length).toBe(total);
  }, 30_000);

  test("unknown_version comes from the newest unknown record that has a version", async () => {
    writeFileSync(
      path,
      [
        JSON.stringify({ type: "user", message: null, version: "2.1.400" }),
        JSON.stringify({ type: "assistant", message: "s", version: "2.1.401" }),
        JSON.stringify({ type: "user", message: { content: 1 } }),
      ]
        .map((l) => `${l}\n`)
        .join(""),
    );
    const r = await ingestFile(ctx(), SID, path, noErr);
    expect(r.unknown).toBe(3);
    expect(store.getMeta("unknown_count")).toBe("3");
    expect(store.getMeta("unknown_version")).toBe("2.1.401");
    appendFileSync(path, `${JSON.stringify({ type: "user", version: "2.2.0" })}\n`);
    await ingestFile(ctx(), SID, path, noErr);
    expect(store.getMeta("unknown_count")).toBe("4");
    expect(store.getMeta("unknown_version")).toBe("2.2.0");
  });

  test("unknown_version is capped: 32 safe chars, else 'invalid'", async () => {
    const cases: [string, string][] = [
      ["2.1.323+build_7-rc", "2.1.323+build_7-rc"],
      ["v".repeat(32), "v".repeat(32)],
      ["v".repeat(33), "invalid"],
      ["2.1 with spaces", "invalid"],
      ["fixture\nline", "invalid"],
    ];
    for (const [version, stored] of cases) {
      appendFileSync(path, `${JSON.stringify({ type: "user", message: null, version })}\n`);
      await ingestFile(ctx(), SID, path, noErr);
      expect(store.getMeta("unknown_version")).toBe(stored);
    }
  });

  test("unrecognised types are counted apart and never raise unknown_count", async () => {
    const types = ["pr-link-v2", "frame.link", "cost_state", "brand-new-type", "pr-link-v2"];
    writeFileSync(
      path,
      types
        .map((type) => `${JSON.stringify({ type, version: "2.3.0", url: "fixture x" })}\n`)
        .join(""),
    );
    const r = await ingestFile(ctx(), SID, path, noErr);
    expect(r.unknown).toBe(0);
    expect(r.unrecognized).toBe(5);
    expect(store.getMeta("unknown_count")).toBeNull();
    expect(store.getMeta("unknown_version")).toBeNull();
    expect(store.getMeta("unrecognized_count")).toBe("5");
    expect(JSON.parse(store.getMeta("unrecognized_types") ?? "null")).toEqual([
      "pr-link-v2",
      "frame.link",
      "cost_state",
      "brand-new-type",
    ]);
    appendFileSync(path, `${JSON.stringify({ type: "another-one" })}\n`);
    await ingestFile(ctx(), SID, path, noErr);
    expect(store.getMeta("unrecognized_count")).toBe("6");
    expect(JSON.parse(store.getMeta("unrecognized_types") ?? "null")).toHaveLength(5);
  });

  test("unrecognized_types keeps at most 10 safe names; unsafe names are counted, not listed", async () => {
    const unsafe = ["", "has space", "x".repeat(41), "fixture\nline", "a/b", "é-type"];
    const safe = Array.from({ length: 12 }, (_, i) => `type-${i}`);
    writeFileSync(
      path,
      [...unsafe, ...safe].map((type) => `${JSON.stringify({ type })}\n`).join(""),
    );
    await ingestFile(ctx(), SID, path, noErr);
    expect(store.getMeta("unrecognized_count")).toBe(String(unsafe.length + safe.length));
    expect(JSON.parse(store.getMeta("unrecognized_types") ?? "null")).toEqual(safe.slice(0, 10));
  });

  test("a 40-character name is listed", async () => {
    writeFileSync(path, `${JSON.stringify({ type: "x".repeat(40) })}\n`);
    await ingestFile(ctx(), SID, path, noErr);
    expect(JSON.parse(store.getMeta("unrecognized_types") ?? "null")).toEqual(["x".repeat(40)]);
  });

  test("a corrupt unrecognized meta is replaced, not trusted", async () => {
    store.setMeta("unrecognized_count", "-7");
    store.setMeta("unrecognized_types", '{"not":"a list"}');
    writeFileSync(path, `${JSON.stringify({ type: "new-type" })}\n`);
    await ingestFile(ctx(), SID, path, noErr);
    expect(store.getMeta("unrecognized_count")).toBe("1");
    expect(JSON.parse(store.getMeta("unrecognized_types") ?? "null")).toEqual(["new-type"]);
    store.setMeta("unrecognized_types", '["ok-type", "bad name", 5]');
    appendFileSync(path, `${JSON.stringify({ type: "next-type" })}\n`);
    await ingestFile(ctx(), SID, path, noErr);
    expect(JSON.parse(store.getMeta("unrecognized_types") ?? "null")).toEqual([
      "ok-type",
      "next-type",
    ]);
  });

  test("a long line with a tiny budget left is read in large chunks, not crawled", async () => {
    // Lines of 1 MiB - 1 byte: two 4 MiB reads take 4 lines each and leave 8 bytes of budget.
    const lineLen = 1024 * 1024 - 1;
    const prefix = '{"type":"queue-operation","pad":"';
    const suffix = '"}';
    const line = `${prefix}${"x".repeat(lineLen - 1 - prefix.length - suffix.length)}${suffix}\n`;
    expect(line.length).toBe(lineLen);
    writeFileSync(path, line.repeat(10));
    const t0 = performance.now();
    const r = await ingestFile(ctx(), SID, path, noErr);
    const ms = performance.now() - t0;
    expect(r.lines).toBe(9);
    expect(r.more).toBe(true);
    expect(store.getTail(path)?.offset).toBe(9 * lineLen);
    // 8-byte reads of the 9th line would take ~131k reads; 64 KiB reads take 16.
    expect(ms).toBeLessThan(1_500);
  }, 30_000);
});

describe("message timestamps are stored as ISO UTC (DATA-07)", () => {
  test("an offset timestamp is converted; a missing one takes the batch time", async () => {
    const line = (uuid: string, timestamp: string | undefined) =>
      JSON.stringify({
        type: "user",
        isSidechain: false,
        uuid,
        ...(timestamp === undefined ? {} : { timestamp }),
        origin: { kind: "human" },
        message: { role: "user", content: `p-${uuid}` },
      });
    writeFileSync(
      path,
      `${[line("a", "2026-11-13T10:00:00+02:00"), line("b", undefined), line("c", "garbage")].join("\n")}\n`,
    );
    const now = Date.parse("2026-11-14T00:00:00.000Z");
    await ingestFile({ ...ctx(), now: () => now }, SID, path, noErr);
    const byUuid = new Map(store.history(SID, 10).map((m) => [m.uuid, m.ts]));
    expect(byUuid.get("a")).toBe("2026-11-13T08:00:00.000Z");
    expect(byUuid.get("b")).toBe("2026-11-13T08:00:00.000Z");
    expect(byUuid.get("c")).toBe("2026-11-13T08:00:00.000Z");
  });
});

describe("masking at ingest", () => {
  test("a peer sender name that carries a secret is masked before it is stored (SEC-11)", async () => {
    const name = `${"ghp"}_${"Q".repeat(36)}`;
    writeFileSync(
      path,
      `${JSON.stringify({
        type: "attachment",
        isSidechain: false,
        uuid: "pn-1",
        timestamp: "2026-11-13T08:00:00.000Z",
        attachment: {
          type: "queued_command",
          prompt: "x",
          origin: { kind: "peer", name, body: "card 4111111111111111", verifiedLocal: true },
        },
      })}\n`,
    );
    await ingestFile(ctx(), SID, path, noErr);
    const [m] = store.history(SID, 10);
    expect(m?.fromName).toBe("[masked:github]");
    expect(m?.text).toBe("card [masked:card]");
  });

  test("an owner message queued while busy is stored once as an owner message, masked", async () => {
    writeFileSync(
      path,
      `${JSON.stringify({
        type: "attachment",
        isSidechain: false,
        uuid: "q-1",
        timestamp: "2026-11-13T08:00:00.000Z",
        attachment: {
          type: "queued_command",
          prompt: "Q1 yes, card 4111111111111111",
          commandMode: "prompt",
          origin: { kind: "human" },
          humanTurn: true,
        },
      })}
`,
    );
    await ingestFile(ctx(), SID, path, noErr);
    await ingestFile(ctx(), SID, path, noErr);
    const rows = store.history(SID, 10);
    expect(rows).toHaveLength(1);
    expect(rows[0]?.role).toBe("owner");
    expect(rows[0]?.fromName).toBeNull();
    expect(rows[0]?.text).toBe("Q1 yes, card [masked:card]");
  });
});
