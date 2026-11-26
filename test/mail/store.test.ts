import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { NewMail } from "../../src/mail/store";
import { type SessionRow, Store } from "../../src/store/db";

let dir: string;
let store: Store;

function session(id: string, over: Partial<SessionRow> = {}): SessionRow {
  return {
    sessionId: id,
    profile: "work",
    name: id,
    cwd: "C:/x",
    pid: 100,
    kind: "interactive",
    status: "idle",
    startedAt: 1000,
    updatedAt: 2000,
    endedAt: null,
    transcriptPath: `C:/x/${id}.jsonl`,
    permissionMode: null,
    ...over,
  };
}

function mail(over: Partial<NewMail> = {}): NewMail {
  return {
    thread: "T-1",
    fromRole: "alpha",
    fromSession: "s-alpha",
    toRole: "beta",
    kind: "task",
    body: "please run the tests",
    evidence: null,
    createdAt: 10_000,
    expiresAt: 100_000,
    ...over,
  };
}

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "hub-mail-"));
  store = Store.open(join(dir, "hub.db"));
});

afterEach(() => {
  store.close();
  rmSync(dir, { recursive: true, force: true });
});

describe("mail", () => {
  test("a new message is queued with its body hash", () => {
    const m = store.mail().insert(mail());
    expect(m.id).toBe(1);
    expect(m.state).toBe("queued");
    expect(m.bodySha).toBe(createHash("sha256").update("please run the tests").digest("hex"));
    expect(store.mail().get(1)).toEqual(m);
  });

  test("the database refuses an unknown kind or state", () => {
    expect(() => store.mail().insert(mail({ kind: "bogus" as NewMail["kind"] }))).toThrow();
  });

  test("claim delivers queued mail for one role to one session, oldest first, once", () => {
    const ms = store.mail();
    ms.insert(mail({ body: "one" }));
    ms.insert(mail({ body: "other role", toRole: "gamma" }));
    ms.insert(mail({ body: "two" }));
    const got = ms.claim("beta", "s-beta", 20_000);
    expect(got.map((m) => m.body)).toEqual(["one", "two"]);
    expect(got.every((m) => m.state === "delivered")).toBe(true);
    expect(got[0]?.deliveredAt).toBe(20_000);
    expect(got[0]?.deliveredSession).toBe("s-beta");
    expect(got[0]?.deliveries).toBe(1);
    expect(ms.claim("beta", "s-other", 21_000)).toEqual([]);
  });

  test("unacked lists mail delivered to a session and not yet acked", () => {
    const ms = store.mail();
    ms.insert(mail({ body: "one" }));
    ms.insert(mail({ body: "two" }));
    ms.claim("beta", "s-beta", 20_000);
    ms.ack(1, "beta", "s-beta", 21_000);
    expect(ms.unackedFor("s-beta").map((m) => m.body)).toEqual(["two"]);
    expect(ms.unackedFor("s-other")).toEqual([]);
  });

  test("ack is only for the recipient role and only once", () => {
    const ms = store.mail();
    ms.insert(mail());
    expect(ms.ack(1, "beta", "s-beta", 15_000)).toBe("not-delivered");
    ms.claim("beta", "s-beta", 20_000);
    expect(ms.ack(1, "alpha", "s-alpha", 21_000)).toBe("not-yours");
    expect(ms.ack(2, "beta", "s-beta", 21_000)).toBe("not-found");
    expect(ms.ack(1, "beta", "s-beta", 22_000)).toBe("ok");
    expect(ms.get(1)?.state).toBe("acked");
    expect(ms.get(1)?.ackedAt).toBe(22_000);
    expect(ms.ack(1, "beta", "s-beta", 23_000)).toBe("already");
  });

  test("only the session the mail was delivered to may ack it; the owner acks mail it took", () => {
    const ms = store.mail();
    ms.insert(mail());
    ms.insert(mail({ toRole: "owner" }));
    ms.claim("beta", "s-beta", 20_000);
    ms.claim("owner", null, 20_000);
    expect(ms.ack(1, "beta", "s-beta-2", 21_000)).toBe("other-session");
    expect(ms.get(1)?.state).toBe("delivered");
    expect(ms.ack(1, "beta", "s-beta", 22_000)).toBe("ok");
    expect(ms.ack(2, "owner", null, 22_000)).toBe("ok");
  });

  test("done closes mail for its sender or recipient; queued mail can be closed too", () => {
    const ms = store.mail();
    ms.insert(mail());
    ms.insert(mail());
    expect(ms.done(1, "gamma", 30_000)).toBe("not-yours");
    expect(ms.done(1, "alpha", 30_000)).toBe("ok");
    expect(ms.get(1)?.state).toBe("done");
    expect(ms.get(1)?.doneAt).toBe(30_000);
    ms.claim("beta", "s-beta", 31_000);
    expect(ms.done(2, "beta", 32_000)).toBe("ok");
    expect(ms.done(2, "beta", 33_000)).toBe("already");
  });

  test("inbox lists open mail for a role; with all it lists every state", () => {
    const ms = store.mail();
    ms.insert(mail({ body: "queued" }));
    ms.insert(mail({ body: "acked" }));
    ms.claim("beta", "s-beta", 20_000);
    ms.ack(2, "beta", "s-beta", 21_000);
    ms.insert(mail({ body: "new" }));
    expect(ms.inbox("beta").map((m) => m.body)).toEqual(["queued", "new"]);
    expect(ms.inbox("beta", { all: true }).map((m) => m.body)).toEqual(["queued", "acked", "new"]);
  });

  test("sent lists a role's mail, newest first", () => {
    const ms = store.mail();
    ms.insert(mail({ body: "a" }));
    ms.insert(mail({ body: "b", fromRole: "gamma" }));
    ms.insert(mail({ body: "c" }));
    expect(ms.sent("alpha", 10).map((m) => m.body)).toEqual(["c", "a"]);
  });

  test("countSentSince counts one session's mail from a time on", () => {
    const ms = store.mail();
    ms.insert(mail({ createdAt: 1 }));
    ms.insert(mail({ createdAt: 5 }));
    ms.insert(mail({ createdAt: 6, fromSession: "other" }));
    expect(ms.countSentSince("s-alpha", 5)).toBe(1);
  });

  test("queuedByRole counts queued mail per role", () => {
    const ms = store.mail();
    ms.insert(mail());
    ms.insert(mail());
    ms.insert(mail({ toRole: "gamma" }));
    expect(ms.queuedByRole()).toEqual(
      new Map([
        ["beta", 2],
        ["gamma", 1],
      ]),
    );
  });
});

describe("mail upkeep", () => {
  test("queued mail past its expiry expires; other states do not", () => {
    const ms = store.mail();
    ms.insert(mail({ expiresAt: 50 }));
    ms.insert(mail({ expiresAt: 50 }));
    ms.insert(mail({ expiresAt: 500 }));
    ms.claim("beta", "s-beta", 20);
    ms.insert(mail({ expiresAt: 50 }));
    const gone = ms.expireQueued(60);
    expect(gone.map((m) => m.id)).toEqual([4]);
    expect(ms.get(4)?.state).toBe("expired");
    expect(ms.get(1)?.state).toBe("delivered");
  });

  test("delivered, unacked mail of an ended session goes back to the queue", () => {
    store.upsertSession(session("s-beta"));
    store.upsertSession(session("s-live"));
    const ms = store.mail();
    ms.insert(mail());
    ms.insert(mail());
    ms.claim("beta", "s-beta", 20);
    ms.ack(2, "beta", "s-beta", 21);
    ms.insert(mail());
    ms.claim("beta", "s-live", 22);
    store.markEnded("s-beta", 30);
    const back = ms.requeueEnded();
    expect(back.map((m) => m.id)).toEqual([1]);
    expect(ms.get(1)?.state).toBe("queued");
    expect(ms.get(1)?.deliveredSession).toBeNull();
    expect(ms.get(1)?.deliveries).toBe(1);
    expect(ms.get(3)?.state).toBe("delivered");
  });

  test("delivered mail to a session the index never saw also goes back", () => {
    const ms = store.mail();
    ms.insert(mail());
    ms.claim("beta", "s-gone", 20);
    expect(ms.requeueEnded().map((m) => m.id)).toEqual([1]);
  });

  test("mail the owner read (no session) stays delivered", () => {
    const ms = store.mail();
    ms.insert(mail({ toRole: "owner" }));
    expect(ms.claim("owner", null, 20)[0]?.deliveredSession).toBeNull();
    expect(ms.requeueEnded()).toEqual([]);
    expect(ms.get(1)?.state).toBe("delivered");
  });

  test("hasQueued tells whether a role has queued mail", () => {
    const ms = store.mail();
    expect(ms.hasQueued("beta")).toBe(false);
    ms.insert(mail());
    expect(ms.hasQueued("beta")).toBe(true);
    ms.claim("beta", "s-beta", 20);
    expect(ms.hasQueued("beta")).toBe(false);
  });

  test("unacked mail past the timeout is flagged once; mail from the hub is never flagged", () => {
    store.upsertSession(session("s-beta"));
    const ms = store.mail();
    ms.insert(mail());
    ms.insert(mail({ fromRole: "hub", fromSession: null }));
    ms.claim("beta", "s-beta", 1_000);
    expect(ms.flagUnacked(1_500, 1_000)).toEqual([]);
    const flagged = ms.flagUnacked(2_000, 1_000);
    expect(flagged.map((m) => m.id)).toEqual([1]);
    expect(ms.get(1)?.flaggedAt).toBe(2_000);
    expect(ms.flagUnacked(9_000, 1_000)).toEqual([]);
  });
});

describe("decisions", () => {
  const ask = {
    role: "alpha",
    question: "Ship it?",
    options: ["yes", "no"],
    recommendation: "yes",
    fromSession: "s-alpha",
    createdAt: 1_000,
  };

  test("ids run D-0001, D-0002 and a new decision is open", () => {
    const ds = store.mail();
    const a = ds.createDecision(ask);
    const b = ds.createDecision({ ...ask, options: [] });
    expect(a.id).toBe("D-0001");
    expect(b.id).toBe("D-0002");
    expect(a.state).toBe("open");
    expect(a.options).toEqual(["yes", "no"]);
    expect(ds.getDecision("D-0001")).toEqual(a);
  });

  test("an answer is stored once; the same answer again changes nothing", () => {
    const ds = store.mail();
    ds.createDecision(ask);
    const r = ds.answerDecision("D-0001", "yes", "tray", 2_000);
    expect(r.result).toBe("answered");
    expect(r.row?.state).toBe("answered");
    expect(r.row?.answer).toBe("yes");
    expect(r.row?.answeredVia).toBe("tray");
    expect(ds.answerDecision("D-0001", "yes", "cli", 3_000).result).toBe("same");
    expect(ds.getDecision("D-0001")?.answeredVia).toBe("tray");
  });

  test("a different second answer is a conflict: kept aside, never applied", () => {
    const ds = store.mail();
    ds.createDecision(ask);
    ds.answerDecision("D-0001", "yes", "tray", 2_000);
    const r = ds.answerDecision("D-0001", "no", "cli", 3_000);
    expect(r.result).toBe("conflict");
    const row = ds.getDecision("D-0001");
    expect(row?.answer).toBe("yes");
    expect(row?.conflicts).toEqual([{ answer: "no", at: 3_000, via: "cli" }]);
  });

  test("replace applies a new answer and keeps the old one in the record", () => {
    const ds = store.mail();
    ds.createDecision(ask);
    ds.answerDecision("D-0001", "yes", "tray", 2_000);
    const r = ds.answerDecision("D-0001", "no", "cli", 3_000, { replace: true });
    expect(r.result).toBe("replaced");
    expect(r.row?.answer).toBe("no");
    expect(r.row?.conflicts).toEqual([{ answer: "yes", at: 2_000, via: "tray" }]);
  });

  test("answering an unknown or closed decision is refused", () => {
    const ds = store.mail();
    expect(ds.answerDecision("D-0009", "yes", "cli", 1).result).toBe("not-found");
    ds.createDecision(ask);
    expect(ds.closeDecision("D-0001", "beta", 2)).toBe("not-yours");
    expect(ds.closeDecision("D-0001", "alpha", 2)).toBe("ok");
    expect(ds.answerDecision("D-0001", "yes", "cli", 3).result).toBe("closed");
    expect(ds.closeDecision("D-0001", "alpha", 4)).toBe("already");
  });

  test("list filters by role and state, oldest first", () => {
    const ds = store.mail();
    ds.createDecision(ask);
    ds.createDecision({ ...ask, role: "beta" });
    ds.createDecision(ask);
    ds.answerDecision("D-0003", "yes", "cli", 5);
    expect(ds.listDecisions({}).map((d) => d.id)).toEqual(["D-0001", "D-0002", "D-0003"]);
    expect(ds.listDecisions({ role: "alpha", state: "open" }).map((d) => d.id)).toEqual(["D-0001"]);
  });
});

describe("retention", () => {
  test("purge drops mail older than the window and settled decisions, keeps open ones", () => {
    const ms = store.mail();
    const day = 86_400_000;
    const now = 30 * day;
    ms.insert(mail({ createdAt: now - 8 * day, expiresAt: now }));
    ms.insert(mail({ createdAt: now - day, expiresAt: now }));
    ms.done(1, "alpha", now - 8 * day);
    const base = { role: "alpha", question: "q", options: [], recommendation: null };
    ms.createDecision({ ...base, fromSession: null, createdAt: now - 9 * day });
    ms.createDecision({ ...base, fromSession: null, createdAt: now - 9 * day });
    ms.answerDecision("D-0002", "ok", "cli", now - 8 * day);
    store.purge(now);
    expect(ms.get(1)).toBeNull();
    expect(ms.get(2)).not.toBeNull();
    expect(ms.getDecision("D-0001")?.state).toBe("open");
    expect(ms.getDecision("D-0002")).toBeNull();
  });
});

describe("ids and retention (review)", () => {
  const day = 86_400_000;
  const base = {
    role: "alpha",
    question: "q",
    options: [],
    recommendation: null,
    fromSession: null,
  };

  test("a decision id is never reused after the newest decision is purged", () => {
    const ms = store.mail();
    ms.createDecision({ ...base, createdAt: 0 });
    ms.createDecision({ ...base, createdAt: 0 });
    ms.answerDecision("D-0002", "ok", "cli", 1);
    store.purge(10 * day);
    expect(ms.getDecision("D-0002")).toBeNull();
    expect(ms.createDecision({ ...base, createdAt: 10 * day }).id).toBe("D-0003");
  });

  test("a mail id is never reused after the newest mail is purged", () => {
    const ms = store.mail();
    ms.insert(mail({ createdAt: 0, expiresAt: 1 }));
    ms.done(1, "alpha", 2);
    store.purge(10 * day);
    expect(ms.get(1)).toBeNull();
    expect(ms.insert(mail({ createdAt: 10 * day, expiresAt: 11 * day })).id).toBe(2);
  });

  test("a decision closed without an answer goes 7 days after it was closed", () => {
    const ms = store.mail();
    ms.createDecision({ ...base, createdAt: 0 });
    ms.closeDecision("D-0001", "alpha", 5 * day);
    store.purge(11 * day);
    expect(ms.getDecision("D-0001")?.state).toBe("closed");
    store.purge(12 * day + 1);
    expect(ms.getDecision("D-0001")).toBeNull();
  });

  test("queued mail is not purged before the upkeep expired it (and told its sender)", () => {
    const ms = store.mail();
    ms.insert(mail({ createdAt: 0, expiresAt: 7 * day }));
    store.purge(8 * day);
    expect(ms.get(1)?.state).toBe("queued");
    ms.expireQueued(8 * day);
    store.purge(8 * day);
    expect(ms.get(1)).toBeNull();
  });
});
