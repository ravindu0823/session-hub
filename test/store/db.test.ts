import { Database } from "bun:sqlite";
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { RETENTION_MS } from "../../src/config";
import { type MessageRow, type SendRow, type SessionRow, Store } from "../../src/store/db";
import { MIGRATIONS } from "../../src/store/schema";

let dir: string;
let dbPath: string;
const opened: Store[] = [];

function open(): Store {
  const s = Store.open(dbPath);
  opened.push(s);
  return s;
}

function session(over: Partial<SessionRow> = {}): SessionRow {
  return {
    sessionId: "s1",
    profile: "work",
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

function msg(uuid: string, ts: string, text: string, sessionId = "s1"): MessageRow {
  return { uuid, sessionId, ts, role: "assistant", fromName: null, text };
}

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "hub-store-"));
  dbPath = join(dir, "index.db");
});

afterEach(() => {
  for (const s of opened.splice(0)) s.close();
  rmSync(dir, { recursive: true, force: true });
});

describe("sessions", () => {
  test("upsert round-trips and updates in place", () => {
    const s = open();
    s.upsertSession(session({ permissionMode: "plan" }));
    s.upsertSession(session({ status: "busy", updatedAt: 3000 }));
    const rows = s.listSessions();
    expect(rows).toHaveLength(1);
    expect(rows[0]).toMatchObject({
      sessionId: "s1",
      state: "busy",
      updatedAt: 3000,
      endedAt: null,
      permissionMode: null,
    });
  });

  test("markEnded sets ended_at; live sessions list first, then profile, name", () => {
    const s = open();
    s.upsertSession(session({ sessionId: "a", profile: "z", name: "n1" }));
    s.upsertSession(session({ sessionId: "b", profile: "a", name: "n2" }));
    s.upsertSession(session({ sessionId: "c", profile: "a", name: "n1" }));
    s.markEnded("c", 5000);
    expect(s.listSessions().map((r) => r.sessionId)).toEqual(["b", "a", "c"]);
    expect(s.listSessions()[2]?.endedAt).toBe(5000);
  });
});

describe("tails", () => {
  test("save and get", () => {
    const s = open();
    expect(s.getTail("p")).toBeNull();
    s.saveTail({ path: "p", offset: 5, size: 9, firstUuid: null });
    s.saveTail({ path: "p", offset: 7, size: 9, firstUuid: "u" });
    expect(s.getTail("p")).toEqual({ path: "p", offset: 7, size: 9, firstUuid: "u" });
  });
});

describe("messages", () => {
  test("dedupe on uuid, history order and paging", () => {
    const s = open();
    s.upsertSession(session());
    s.addMessages("s1", [
      msg("u1", "2026-01-01T00:00:01Z", "one"),
      msg("u2", "2026-01-01T00:00:02Z", "two"),
      msg("u3", "2026-01-01T00:00:03Z", "three"),
    ]);
    s.addMessages("s1", [
      msg("u2", "2026-01-01T00:00:02Z", "dup"),
      msg("u4", "2026-01-01T00:00:04Z", "four"),
    ]);
    expect(s.history("s1", 10).map((m) => m.uuid)).toEqual(["u1", "u2", "u3", "u4"]);
    expect(s.history("s1", 10).find((m) => m.uuid === "u2")?.text).toBe("two");
    expect(s.history("s1", 2).map((m) => m.uuid)).toEqual(["u3", "u4"]);
    expect(s.history("s1", 2, "2026-01-01T00:00:03Z").map((m) => m.uuid)).toEqual(["u1", "u2"]);
  });

  test("paging with a (ts, uuid) cursor never skips or repeats rows that share a ts", () => {
    const s = open();
    s.upsertSession(session());
    s.addMessages("s1", [
      msg("t1", "2026-01-01T00:00:02Z", "one"),
      msg("t2", "2026-01-01T00:00:02Z", "two"),
      msg("t3", "2026-01-01T00:00:03Z", "three"),
    ]);
    const first = s.history("s1", 2);
    expect(first.map((m) => m.uuid)).toEqual(["t2", "t3"]);
    const oldest = first[0];
    if (oldest === undefined) throw new Error("no rows");
    // The ts-only cursor skips the row that shares the boundary ts (old behaviour, kept).
    expect(s.history("s1", 2, oldest.ts).map((m) => m.uuid)).toEqual([]);
    const second = s.history("s1", 2, oldest.ts, oldest.uuid);
    expect(second.map((m) => m.uuid)).toEqual(["t1"]);
    const all = [...second, ...first].map((m) => m.uuid);
    expect(all).toEqual(["t1", "t2", "t3"]);
    expect(new Set(all).size).toBe(3);
  });

  test("a cursor of three rows on one ts pages through all of them", () => {
    const s = open();
    s.upsertSession(session());
    s.addMessages("s1", [
      msg("a", "2026-01-01T00:00:05Z", "a"),
      msg("b", "2026-01-01T00:00:05Z", "b"),
      msg("c", "2026-01-01T00:00:05Z", "c"),
    ]);
    const p1 = s.history("s1", 2);
    expect(p1.map((m) => m.uuid)).toEqual(["b", "c"]);
    const p2 = s.history("s1", 2, p1[0]?.ts, p1[0]?.uuid);
    expect(p2.map((m) => m.uuid)).toEqual(["a"]);
    // An unknown uuid falls back to the ts-only cursor.
    expect(s.history("s1", 2, p1[0]?.ts, "nope")).toEqual([]);
  });

  test("history keeps role and fromName", () => {
    const s = open();
    s.upsertSession(session());
    s.addMessages("s1", [
      { uuid: "p", sessionId: "s1", ts: "t", role: "peer", fromName: "beta", text: "hi" },
    ]);
    expect(s.history("s1", 5)[0]).toMatchObject({ role: "peer", fromName: "beta" });
  });
});

describe("search", () => {
  test("finds a word, snippet markers, newest first, implicit AND", () => {
    const s = open();
    s.upsertSession(session());
    s.addMessages("s1", [
      msg("u1", "2026-01-01T00:00:01Z", "deploy the migration tonight"),
      msg("u2", "2026-01-01T00:00:02Z", "unrelated"),
      msg("u3", "2026-01-01T00:00:03Z", "migration finished"),
    ]);
    const hits = s.search("migration", 10);
    expect(hits.map((h) => h.uuid)).toEqual(["u3", "u1"]);
    expect(hits[0]?.snippet).toContain("[migration]");
    expect(hits[0]).toMatchObject({
      sessionId: "s1",
      role: "assistant",
      ts: "2026-01-01T00:00:03Z",
    });
    expect(s.search("migration tonight", 10).map((h) => h.uuid)).toEqual(["u1"]);
  });

  test("special characters do not break the query; empty gives []", () => {
    const s = open();
    s.upsertSession(session());
    s.addMessages("s1", [msg("u1", "t", 'say "hello" now')]);
    expect(s.search("", 5)).toEqual([]);
    expect(s.search("   ", 5)).toEqual([]);
    expect(s.search('AND OR ( * "', 5)).toEqual([]);
    expect(s.search('"hello"', 5).map((h) => h.uuid)).toEqual(["u1"]);
  });
});

describe("status", () => {
  test("setStatus, setLatestReply, SessionView", () => {
    const s = open();
    s.upsertSession(session());
    expect(s.listSessions()[0]).toMatchObject({
      state: "idle",
      status: null,
      latestReply: null,
      waitingCount: 0,
    });
    s.setLatestReply("s1", "r1", "ts1", false);
    expect(s.listSessions()[0]?.latestReply).toEqual({
      replyUuid: "r1",
      replyTs: "ts1",
      hasStatus: false,
    });
    s.setStatus(
      "s1",
      {
        progressPct: 40,
        phase: "build",
        remaining: ["a", "b"],
        waiting: [{ id: "Q1", text: "ok?", isNew: true, children: ["x"] }],
        recommendations: ["do it"],
      },
      "r2",
      "ts2",
    );
    s.setLatestReply("s1", "r2", "ts2", true);
    const v = s.listSessions()[0];
    expect(v?.waitingCount).toBe(1);
    expect(v?.latestReply).toEqual({ replyUuid: "r2", replyTs: "ts2", hasStatus: true });
    expect(v?.status).toEqual({
      progressPct: 40,
      phase: "build",
      remaining: ["a", "b"],
      waiting: [{ id: "Q1", text: "ok?", isNew: true, children: ["x"], answered: false }],
      recommendations: ["do it"],
      replyUuid: "r2",
      replyTs: "ts2",
    });
  });

  test("items named by the owner or the relay after the status reply are answered", () => {
    const s = open();
    s.upsertSession(session());
    s.upsertSession(session({ sessionId: "s2", name: "beta", transcriptPath: "C:/x/s2.jsonl" }));
    const st = (ids: (string | null)[]) => ({
      progressPct: null,
      phase: null,
      remaining: [],
      waiting: ids.map((id, i) => ({ id, text: `q${i}`, isNew: false, children: [] })),
      recommendations: [],
    });
    s.setStatus("s1", st(["Q1", "Q2", "Q3", null]), "r1", "2026-11-14T10:00:00.000Z");
    s.setStatus("s2", st(["Q1"]), "r9", "2026-11-14T10:00:00.000Z");
    const row = (
      uuid: string,
      ts: string,
      role: MessageRow["role"],
      text: string,
      fromName: string | null = null,
      sessionId = "s1",
    ): MessageRow => ({ uuid, sessionId, ts, role, fromName, text });
    s.addMessages("s1", [
      // before the status reply: does not count
      row("m0", "2026-11-14T09:59:00.000Z", "owner", "Q3 yes"),
      // the owner's terminal answer and a relay answer after it
      row("m1", "2026-11-14T10:01:00.000Z", "owner", "Q1 Ignore that"),
      row("m2", "2026-11-14T10:02:00.000Z", "peer", "Answers:\nQ2: go", "session-hub-relay"),
      // another peer naming Q3 and the assistant naming Q3 do not answer it
      row("m3", "2026-11-14T10:03:00.000Z", "peer", "Q3: done", "some-session"),
      row("m4", "2026-11-14T10:04:00.000Z", "assistant", "Q3 still open"),
    ]);
    const v = s.listSessions().find((x) => x.sessionId === "s1");
    expect(v?.status?.waiting.map((w) => [w.id, w.answered])).toEqual([
      ["Q1", true],
      ["Q2", true],
      ["Q3", false],
      [null, false],
    ]);
    expect(v?.waitingCount).toBe(2);
    // another session's messages never answer this one's items
    expect(s.listSessions().find((x) => x.sessionId === "s2")?.status?.waiting[0]?.answered).toBe(
      false,
    );
    // an ended session is never counted, so its items are not marked
    s.upsertSession(session({ endedAt: 9000 }));
    expect(s.listSessions().find((x) => x.sessionId === "s1")?.status?.waiting[0]?.answered).toBe(
      false,
    );
    s.upsertSession(session());
    // a new status reply starts over
    s.setStatus("s1", st(["Q1"]), "r3", "2026-11-14T10:05:00.000Z");
    const w = s.listSessions().find((x) => x.sessionId === "s1");
    expect(w?.status?.waiting[0]?.answered).toBe(false);
    expect(w?.waitingCount).toBe(1);
  });
});

describe("meta", () => {
  test("set, get, overwrite, null deletes", () => {
    const s = open();
    expect(s.getMeta("k")).toBeNull();
    s.setMeta("k", "v");
    expect(s.getMeta("k")).toBe("v");
    s.setMeta("k", "w");
    expect(s.getMeta("k")).toBe("w");
    s.setMeta("k", null);
    expect(s.getMeta("k")).toBeNull();
  });
});

describe("sends", () => {
  test("create, openSendFor, update", () => {
    const s = open();
    const base: SendRow = {
      sessionId: "s1",
      profile: "work",
      name: "alpha",
      sha256: "h",
      createdAt: 1,
      state: "pending",
      detail: null,
    };
    const a = s.createSend(base);
    const b = s.createSend({ ...base, createdAt: 2, state: "held" });
    expect(b).toBeGreaterThan(a);
    expect(s.openSendFor("s1")?.id).toBe(b);
    expect(s.transitionSend(b, "delivered", "ok")).toBe(true);
    expect(s.getSend(b)?.detail).toBe("ok");
    expect(s.openSendFor("s1")?.id).toBe(a);
    expect(s.transitionSend(a, "expired")).toBe(true);
    expect(s.openSendFor("s1")).toBeNull();
    expect(s.openSendFor("nope")).toBeNull();
  });

  test("getSend reads one send in any state", () => {
    const s = open();
    const id = s.createSend({
      sessionId: "s1",
      profile: "work",
      name: "alpha",
      sha256: "h",
      createdAt: 5,
      state: "pending",
      detail: null,
    });
    expect(s.getSend(id)?.state).toBe("pending");
    s.transitionSend(id, "delivered", "ok");
    expect(s.getSend(id)).toEqual({
      id,
      sessionId: "s1",
      profile: "work",
      name: "alpha",
      sha256: "h",
      createdAt: 5,
      state: "delivered",
      detail: "ok",
    });
    expect(s.getSend(id + 1)).toBeNull();
  });
});

describe("retention leaves no residue (PII-03)", () => {
  const iso = (ms: number) => new Date(ms).toISOString();

  test("messages older than the window go from live sessions too; newer ones stay", () => {
    const s = open();
    const now = 10 * RETENTION_MS;
    s.upsertSession(session({ sessionId: "live", transcriptPath: "live.jsonl" }));
    s.addMessages("live", [
      msg("old-1", iso(now - RETENTION_MS - 60_000), "walrusold", "live"),
      msg("new-1", iso(now - 60_000), "walrusnew", "live"),
    ]);
    s.purge(now);
    expect(s.listSessions().map((r) => r.sessionId)).toEqual(["live"]);
    expect(s.history("live", 10).map((m) => m.uuid)).toEqual(["new-1"]);
    expect(s.search("walrusold", 10)).toEqual([]);
    expect(s.search("walrusnew", 10)).toHaveLength(1);
  });

  test("a live session's status and latest reply older than the window go too", () => {
    const s = open();
    const now = 10 * RETENTION_MS;
    s.upsertSession(session({ sessionId: "live", transcriptPath: "live.jsonl" }));
    s.upsertSession(session({ sessionId: "fresh", transcriptPath: "fresh.jsonl" }));
    const st = {
      progressPct: 5,
      phase: "old phase text",
      remaining: ["r"],
      waiting: [],
      recommendations: [],
    };
    s.setStatus("live", st, "r-old", iso(now - RETENTION_MS - 1000));
    s.setLatestReply("live", "r-old", iso(now - RETENTION_MS - 1000), true);
    s.setStatus("fresh", st, "r-new", iso(now - 1000));
    s.setLatestReply("fresh", "r-new", iso(now - 1000), true);
    s.purge(now);
    const byId = new Map(s.listSessions().map((r) => [r.sessionId, r]));
    expect(byId.get("live")?.status).toBeNull();
    expect(byId.get("live")?.latestReply).toBeNull();
    expect(byId.get("fresh")?.status?.phase).toBe("old phase text");
  });

  test("a large backlog is purged in batches and nothing old is left", () => {
    const s = open();
    const now = 10 * RETENTION_MS;
    s.upsertSession(session({ sessionId: "live", transcriptPath: "live.jsonl" }));
    s.addMessages(
      "live",
      Array.from({ length: 1234 }, (_, i) =>
        msg(`b-${i}`, iso(now - RETENTION_MS - 10_000 + i), `old ${i}`, "live"),
      ),
    );
    s.purge(now);
    expect(s.history("live", 5000)).toEqual([]);
  });

  test("purged text is not left in the database or WAL files after the vacuum", () => {
    const s = open();
    const now = 10 * RETENTION_MS;
    s.upsertSession(session({ sessionId: "old", transcriptPath: "old.jsonl" }));
    s.upsertSession(session({ sessionId: "live", transcriptPath: "live.jsonl" }));
    s.markEnded("old", now - RETENTION_MS - 1);
    const canary = "quokkacanary7731";
    s.addMessages("old", [msg("c-1", iso(now - RETENTION_MS - 5), `${canary} ${canary}`, "old")]);
    s.addMessages(
      "live",
      Array.from({ length: 50 }, (_, i) => msg(`k-${i}`, iso(now - 1000), `keep ${i}`, "live")),
    );
    expect(s.search(canary, 10)).toHaveLength(1);
    s.purge(now);
    s.vacuum();
    const bytes = [dbPath, `${dbPath}-wal`]
      .filter((p) => existsSync(p))
      .map((p) => readFileSync(p).toString("latin1"))
      .join("");
    expect(bytes.includes(canary)).toBe(false);
    expect(s.search("keep", 100)).toHaveLength(50);
  });
});

describe("purge leaves no WAL residue (PII-03)", () => {
  const iso = (ms: number) => new Date(ms).toISOString();
  test("purged text is gone from the database and WAL files right after the purge", () => {
    const s = open();
    const now = 10 * RETENTION_MS;
    s.upsertSession(session({ sessionId: "live", transcriptPath: "live.jsonl" }));
    const canary = "wombatcanary4417";
    s.addMessages("live", [msg("c-1", iso(now - RETENTION_MS - 5), `${canary} ${canary}`, "live")]);
    s.addMessages("live", [msg("k-1", iso(now - 1000), "keep this", "live")]);
    expect(s.search(canary, 10)).toHaveLength(1);
    s.purge(now);
    const bytes = [dbPath, `${dbPath}-wal`]
      .filter((p) => existsSync(p))
      .map((p) => readFileSync(p).toString("latin1"))
      .join("");
    expect(bytes.includes(canary)).toBe(false);
    expect(s.search("keep", 10)).toHaveLength(1);
  });
});

describe("purge", () => {
  test("removes old ended sessions with messages/status/tails/fts, keeps live and recent", () => {
    const s = open();
    const now = 10 * RETENTION_MS;
    s.upsertSession(session({ sessionId: "old", transcriptPath: "old.jsonl" }));
    s.upsertSession(session({ sessionId: "recent", transcriptPath: "recent.jsonl" }));
    s.upsertSession(session({ sessionId: "live", transcriptPath: "live.jsonl" }));
    s.markEnded("old", now - RETENTION_MS - 1);
    s.markEnded("recent", now - RETENTION_MS + 1000);
    for (const id of ["old", "recent", "live"]) {
      s.addMessages(id, [msg(`m-${id}`, "t", "zebra", id)]);
      s.setLatestReply(id, "r", "t", false);
      s.setStatus(
        id,
        { progressPct: null, phase: null, remaining: [], waiting: [], recommendations: [] },
        "r",
        "t",
      );
      s.saveTail({ path: `${id}.jsonl`, offset: 1, size: 1, firstUuid: null });
    }
    const send: SendRow = {
      sessionId: "old",
      profile: "p",
      name: "n",
      sha256: "h",
      state: "delivered",
      detail: null,
      createdAt: now - RETENTION_MS - 1,
    };
    s.createSend(send);
    const keep = s.createSend({ ...send, state: "pending", createdAt: now - 1000 });

    expect(s.purge(now)).toBe(1);
    expect(
      s
        .listSessions()
        .map((r) => r.sessionId)
        .sort(),
    ).toEqual(["live", "recent"]);
    expect(s.history("old", 10)).toEqual([]);
    expect(s.getTail("old.jsonl")).toBeNull();
    expect(s.getTail("live.jsonl")).not.toBeNull();
    expect(
      s
        .search("zebra", 10)
        .map((h) => h.sessionId)
        .sort(),
    ).toEqual(["live", "recent"]);
    expect(s.openSendFor("old")?.id).toBe(keep);
    s.transitionSend(keep, "failed");
    s.vacuum();
    expect(s.search("zebra", 10)).toHaveLength(2);
  });
});

describe("migrations", () => {
  test("re-open is idempotent and keeps data", () => {
    const a = open();
    a.upsertSession(session());
    a.close();
    opened.pop();
    const b = open();
    expect(b.listSessions()).toHaveLength(1);
    const c = open();
    expect(c.listSessions()).toHaveLength(1);
  });

  test("v2 resets the inflated unknown_count of a v1 index once, keeping other meta", () => {
    const raw = new Database(dbPath, { create: true, strict: true });
    raw.run("CREATE TABLE schema_version (version INTEGER NOT NULL)");
    raw.run(MIGRATIONS[0] as string);
    raw.run("INSERT INTO schema_version (version) VALUES (1)");
    raw.run(
      "INSERT INTO meta (key, value) VALUES ('unknown_count', '1441'), ('unknown_version', '2.1.300'), ('stale_since:claude', '5')",
    );
    raw.close();
    const a = open();
    expect(a.getMeta("unknown_count")).toBe("0");
    expect(a.getMeta("unknown_version")).toBe("2.1.300");
    expect(a.getMeta("stale_since:claude")).toBe("5");
    a.setMeta("unknown_count", "2");
    a.close();
    opened.pop();
    expect(open().getMeta("unknown_count")).toBe("2");
  });

  test("a fresh index is at the latest schema version with no unknown_count", () => {
    const s = open();
    expect(s.getMeta("unknown_count")).toBeNull();
    const raw = new Database(dbPath, { readonly: true });
    const row = raw.query("SELECT MAX(version) AS v FROM schema_version").get() as { v: number };
    raw.close();
    expect(row.v).toBe(MIGRATIONS.length);
    expect(MIGRATIONS.length).toBe(4);
  });

  test("v3 turns on full-text secure-delete for a v2 index and keeps its messages searchable", () => {
    const raw = new Database(dbPath, { create: true, strict: true });
    raw.run("CREATE TABLE schema_version (version INTEGER NOT NULL)");
    raw.run(MIGRATIONS[0] as string);
    raw.run(MIGRATIONS[1] as string);
    raw.run("INSERT INTO schema_version (version) VALUES (1), (2)");
    raw.run(
      "INSERT INTO sessions VALUES ('s1','work','alpha','C:/x',1,'interactive','idle',1,2,NULL,'C:/x/s1.jsonl',NULL)",
    );
    raw.run(
      "INSERT INTO messages (uuid, session_id, ts, role, from_name, text) VALUES ('u1','s1','2026-11-13T00:00:00.000Z','assistant',NULL,'pelican')",
    );
    raw.close();
    const s = open();
    expect(s.search("pelican", 10)).toHaveLength(1);
    const check = new Database(dbPath, { readonly: true });
    const cfg = check
      .query("SELECT v FROM messages_fts_config WHERE k = 'secure-delete'")
      .get() as { v: number } | null;
    check.close();
    expect(cfg?.v).toBe(1);
  });
});

describe("v4: stored timestamps become ISO UTC (DATA-07)", () => {
  // A v3 index written before ingest normalised timestamps: empty, offset and unparseable values.
  function v3Index(now: number): void {
    const raw = new Database(dbPath, { create: true, strict: true });
    raw.run("CREATE TABLE schema_version (version INTEGER NOT NULL)");
    for (const m of MIGRATIONS.slice(0, 3)) raw.run(m);
    raw.run("INSERT INTO schema_version (version) VALUES (1), (2), (3)");
    raw.run(
      `INSERT INTO sessions VALUES ('s1','work','alpha','C:/x',1,'interactive','idle',1,${now},NULL,'C:/x/s1.jsonl',NULL)`,
    );
    const ins = raw.query(
      "INSERT INTO messages (uuid, session_id, ts, role, from_name, text) VALUES (?, 's1', ?, 'assistant', NULL, ?)",
    );
    ins.run("empty", "", "recent but undated");
    ins.run("offset", "2026-11-14T10:00:00+05:30", "recent with an offset");
    ins.run("old", "2020-01-01T00:00:00+05:30", "years old");
    ins.run("junk", "not a date", "unparseable");
    ins.run("iso", "2026-11-14T04:00:00.000Z", "already ISO");
    raw.run("INSERT INTO status VALUES ('s1', 50, 'p', '[]', '[]', '[]', 'empty', '')");
    raw.run("INSERT INTO latest_reply VALUES ('s1', 'offset', '2026-11-14T10:00:00+05:30', 1)");
    raw.close();
  }

  test("normalises messages, status and latest_reply; empty or unparseable take updated_at", () => {
    const now = Date.parse("2026-11-14T06:00:00.123Z");
    v3Index(now);
    open().close();
    opened.pop();
    const raw = new Database(dbPath, { readonly: true });
    const ts = Object.fromEntries(
      (raw.query("SELECT uuid, ts FROM messages").all() as { uuid: string; ts: string }[]).map(
        (r) => [r.uuid, r.ts],
      ),
    );
    const st = raw.query("SELECT reply_ts AS t FROM status").get() as { t: string };
    const lr = raw.query("SELECT reply_ts AS t FROM latest_reply").get() as { t: string };
    raw.close();
    const fallback = new Date(now).toISOString();
    expect(ts).toEqual({
      empty: fallback,
      offset: "2026-11-14T04:30:00.000Z",
      old: "2019-12-31T18:30:00.000Z",
      junk: fallback,
      iso: "2026-11-14T04:00:00.000Z",
    });
    expect(st.t).toBe(fallback);
    expect(lr.t).toBe("2026-11-14T04:30:00.000Z");
  });

  test("the first purge after the upgrade keeps recent rows and drops only old ones", () => {
    const now = Date.parse("2026-11-14T06:00:00.123Z");
    v3Index(now);
    const s = open();
    s.purge(now);
    expect(
      s
        .history("s1", 50)
        .map((m) => m.uuid)
        .sort(),
    ).toEqual(["empty", "iso", "junk", "offset"]);
    const view = s.listSessions()[0];
    expect(view?.status).not.toBeNull();
    expect(view?.latestReply).not.toBeNull();
  });

  test("the full-text index still finds a message whose timestamp changed", () => {
    v3Index(Date.parse("2026-11-14T06:00:00.123Z"));
    expect(open().search("offset", 10)).toHaveLength(1);
  });

  test("the full-text update trigger fires only on text changes, and still re-indexes them", () => {
    v3Index(Date.parse("2026-11-14T06:00:00.123Z"));
    open().close();
    opened.pop();
    const raw = new Database(dbPath, { strict: true });
    const sql = raw.query("SELECT sql FROM sqlite_master WHERE name = 'messages_au'").get() as {
      sql: string;
    };
    expect(sql.sql).toContain("AFTER UPDATE OF text ON messages");
    raw.run("UPDATE messages SET text = 'heron' WHERE uuid = 'offset'");
    raw.close();
    const s = open();
    expect(s.search("heron", 10)).toHaveLength(1);
    expect(s.search("offset", 10)).toEqual([]);
  });

  test("a row whose session is missing takes the migration time, so the open never fails", () => {
    const raw = new Database(dbPath, { create: true, strict: true });
    raw.run("CREATE TABLE schema_version (version INTEGER NOT NULL)");
    for (const m of MIGRATIONS.slice(0, 3)) raw.run(m);
    raw.run("INSERT INTO schema_version (version) VALUES (1), (2), (3)");
    raw.run("PRAGMA foreign_keys = OFF");
    raw.run(
      "INSERT INTO messages (uuid, session_id, ts, role, from_name, text) VALUES ('orphan', 'gone', '', 'assistant', NULL, 'x')",
    );
    raw.close();
    const before = Date.now();
    open().close();
    opened.pop();
    const check = new Database(dbPath, { readonly: true });
    const row = check.query("SELECT ts FROM messages WHERE uuid = 'orphan'").get() as {
      ts: string;
    };
    check.close();
    expect(Date.parse(row.ts)).toBeGreaterThanOrEqual(before - 1000);
    expect(row.ts).toMatch(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/);
  });
});

describe("concurrency", () => {
  test("daemon in a separate process writes while this process reads: no SQLITE_BUSY", async () => {
    const reader = open();
    const child = Bun.spawn([process.execPath, join(import.meta.dir, "writer.ts"), dbPath], {
      stdout: "pipe",
      stderr: "pipe",
    });
    let loops = 0;
    while (child.exitCode === null) {
      reader.listSessions();
      reader.history("s1", 20);
      reader.search("common", 5);
      loops++;
      await Bun.sleep(0);
    }
    const err = await new Response(child.stderr).text();
    expect(err).toBe("");
    expect(child.exitCode).toBe(0);
    expect(loops).toBeGreaterThan(0);
    expect(reader.history("s1", 5000)).toHaveLength(2000);
    expect(reader.search("common", 5000)).toHaveLength(2000);
    expect(reader.search("ancient", 5)).toEqual([]);
    expect(reader.listSessions().map((r) => r.sessionId)).toEqual(["s1"]);
  });

  test("two handles writing interleaved", async () => {
    const a = open();
    const b = open();
    a.upsertSession(session());
    const job = async (st: Store, tag: string) => {
      for (let i = 0; i < 100; i++) {
        st.addMessages("s1", [msg(`${tag}${i}`, "t", "x")]);
        await Promise.resolve();
      }
    };
    await Promise.all([job(a, "a"), job(b, "b")]);
    expect(a.history("s1", 1000)).toHaveLength(200);
  });
});

describe("tx", () => {
  test("commits together and rolls back on throw", () => {
    const s = open();
    s.tx(() => {
      s.upsertSession(session());
      s.setLatestReply("s1", "r", "t", false);
    });
    expect(s.listSessions()[0]?.latestReply).not.toBeNull();
    expect(() =>
      s.tx(() => {
        s.upsertSession(session({ sessionId: "s2" }));
        throw new Error("boom");
      }),
    ).toThrow("boom");
    expect(s.listSessions().map((r) => r.sessionId)).toEqual(["s1"]);
    s.addMessages("s1", [msg("after", "t", "still works")]);
    expect(s.history("s1", 5)).toHaveLength(1);
  });
});

describe("open guards", () => {
  test("newer schema_version throws and releases the file", () => {
    open().close();
    opened.pop();
    const raw = new Database(dbPath);
    raw.run("INSERT INTO schema_version (version) VALUES (999)");
    raw.close();
    expect(() => Store.open(dbPath)).toThrow(/newer/);
    rmSync(dbPath);
    rmSync(`${dbPath}-wal`, { force: true });
    rmSync(`${dbPath}-shm`, { force: true });
  });
});

describe("open without create", () => {
  test("create:false on a missing path throws and creates no file", () => {
    expect(() => Store.open(dbPath, { create: false })).toThrow();
    expect(existsSync(dbPath)).toBe(false);
    expect(existsSync(`${dbPath}-wal`)).toBe(false);
  });

  test("create:false opens an existing index", () => {
    const a = open();
    a.upsertSession(session());
    const b = Store.open(dbPath, { create: false });
    opened.push(b);
    expect(b.listSessions().map((x) => x.sessionId)).toEqual(["s1"]);
  });
});

describe("fts consistency", () => {
  function ftsDocs(): number {
    const raw = new Database(dbPath, { readonly: true });
    try {
      return (raw.query("SELECT COUNT(*) AS n FROM messages_fts_docsize").get() as { n: number }).n;
    } finally {
      raw.close();
    }
  }

  test("dedupe does not double-index; purge removes fts rows; vacuum keeps search", () => {
    const s = open();
    s.upsertSession(session({ sessionId: "old", transcriptPath: "o" }));
    s.upsertSession(session({ sessionId: "live", transcriptPath: "l" }));
    s.addMessages("old", [msg("a", "t", "zebra", "old")]);
    s.addMessages("live", [msg("b", "t", "zebra", "live")]);
    s.addMessages("live", [msg("b", "t", "zebra", "live")]);
    expect(ftsDocs()).toBe(2);
    s.markEnded("old", 1);
    expect(s.purge(RETENTION_MS * 10)).toBe(1);
    expect(ftsDocs()).toBe(1);
    s.vacuum();
    expect(ftsDocs()).toBe(1);
    expect(s.search("zebra", 5).map((h) => h.uuid)).toEqual(["b"]);
  });
});

describe("purge keeps shared tails", () => {
  test("tail used by a live session survives purge of an old session on the same path", () => {
    const s = open();
    const now = 10 * RETENTION_MS;
    s.upsertSession(session({ sessionId: "old", transcriptPath: "shared" }));
    s.upsertSession(session({ sessionId: "live", transcriptPath: "shared" }));
    s.upsertSession(session({ sessionId: "gone", transcriptPath: "own" }));
    s.markEnded("old", 1);
    s.markEnded("gone", 1);
    s.saveTail({ path: "shared", offset: 1, size: 1, firstUuid: null });
    s.saveTail({ path: "own", offset: 1, size: 1, firstUuid: null });
    expect(s.purge(now)).toBe(2);
    expect(s.getTail("shared")).not.toBeNull();
    expect(s.getTail("own")).toBeNull();
  });
});

describe("dataVersion", () => {
  test("changes after another connection commits, not after own write", () => {
    const a = open();
    const b = open();
    const v0 = a.dataVersion();
    b.setMeta("x", "1");
    const v1 = a.dataVersion();
    expect(v1).not.toBe(v0);
    a.setMeta("y", "2");
    expect(a.dataVersion()).toBe(v1);
  });
});
