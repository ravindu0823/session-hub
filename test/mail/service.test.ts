import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { parseRolesConfig, type RolesConfig } from "../../src/mail/roles";
import {
  answerAndNotify,
  type Caller,
  MAX_MAIL_PER_MINUTE,
  MailRefused,
  mailUpkeep,
  OPEN_DECISION_MAX_MS,
  resolveCaller,
  sendMail,
} from "../../src/mail/service";
import { type SessionRow, Store } from "../../src/store/db";

let dir: string;
let store: Store;
let cfg: RolesConfig;

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

const alpha: Caller = { role: "alpha", sessionId: "s-alpha" };

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "hub-mailsvc-"));
  store = Store.open(join(dir, "hub.db"));
  cfg = parseRolesConfig(
    JSON.stringify({
      roles: [
        { role: "alpha", profile: "work", sessions: ["alpha", "alpha-*"] },
        { role: "beta", profile: "home" },
      ],
    }),
  );
});

afterEach(() => {
  store.close();
  rmSync(dir, { recursive: true, force: true });
});

describe("resolveCaller", () => {
  test("outside Claude Code the caller is the owner", async () => {
    expect(await resolveCaller(store, cfg, {}, { waitMs: 0 })).toEqual({
      role: "owner",
      sessionId: null,
    });
  });

  test("inside a session the caller is that session's role", async () => {
    store.upsertSession(session("sid-1", { name: "alpha-2" }));
    const c = await resolveCaller(store, cfg, { CLAUDE_CODE_SESSION_ID: "sid-1" }, { waitMs: 0 });
    expect(c).toEqual({ role: "alpha", sessionId: "sid-1" });
  });

  test("a session the index does not know yet is waited for, then refused", async () => {
    let calls = 0;
    const r = resolveCaller(
      store,
      cfg,
      { CLAUDE_CODE_SESSION_ID: "sid-new" },
      {
        waitMs: 1_000,
        sleep: async () => {
          calls += 1;
          if (calls === 2) store.upsertSession(session("sid-new", { name: "gamma" }));
        },
      },
    );
    expect(await r).toEqual({ role: "gamma", sessionId: "sid-new" });
    await expect(
      resolveCaller(store, cfg, { CLAUDE_CODE_SESSION_ID: "sid-x" }, { waitMs: 0 }),
    ).rejects.toThrow(MailRefused);
  });

  test("an ended session or one without a role name is refused", async () => {
    store.upsertSession(session("sid-e", { endedAt: 5 }));
    store.upsertSession(session("sid-w", { name: "two words" }));
    for (const id of ["sid-e", "sid-w"]) {
      await expect(
        resolveCaller(store, cfg, { CLAUDE_CODE_SESSION_ID: id }, { waitMs: 0 }),
      ).rejects.toThrow(MailRefused);
    }
  });
});

describe("sendMail", () => {
  const base = { to: "beta", thread: "T-1", kind: "task" as const, body: "run it", evidence: null };

  test("queues mail from the caller's role with the configured expiry", () => {
    const m = sendMail(store, cfg, alpha, base, 1_000);
    expect(m.fromRole).toBe("alpha");
    expect(m.fromSession).toBe("s-alpha");
    expect(m.toRole).toBe("beta");
    expect(m.expiresAt).toBe(1_000 + cfg.expiryMs);
    expect(m.state).toBe("queued");
  });

  test("a ttl overrides the expiry", () => {
    expect(sendMail(store, cfg, alpha, { ...base, ttlHours: 2 }, 0).expiresAt).toBe(7_200_000);
  });

  function refusal(fn: () => unknown): string {
    try {
      fn();
    } catch (e) {
      if (e instanceof MailRefused) return e.reason;
      throw e;
    }
    return "sent";
  }

  test("refuses bad input with a reason", () => {
    const cases: [Record<string, unknown>, string][] = [
      [{ to: "Nope" }, "bad-role"],
      [{ to: "zeta" }, "unknown-role"],
      [{ to: "hub" }, "unknown-role"],
      [{ to: "alpha" }, "own-role"],
      [{ thread: "" }, "bad-thread"],
      [{ thread: "has space" }, "bad-thread"],
      [{ kind: "bogus" }, "bad-kind"],
      [{ body: "  \n" }, "empty-body"],
      [{ body: "x".repeat(20_001) }, "too-long"],
      [{ kind: "answer" }, "evidence-required"],
      [{ kind: "chat-out", evidence: " " }, "evidence-required"],
      [{ evidence: "e".repeat(2_001), kind: "answer" }, "too-long"],
      [{ body: "token ghp_abcdefghijklmnopqrstuvwxyz0123456789" }, "looks-secret"],
      [{ kind: "answer", evidence: "Bearer abcdefghijklmnopqrstuvwxyz" }, "looks-secret"],
      [{ ttlHours: 0 }, "bad-ttl"],
      [{ ttlHours: 169 }, "bad-ttl"],
    ];
    for (const [over, reason] of cases) {
      expect(refusal(() => sendMail(store, cfg, alpha, { ...base, ...over } as never, 1))).toBe(
        reason,
      );
    }
  });

  test("a role known only from an indexed session is a valid recipient", () => {
    store.upsertSession(session("sid-g", { name: "gamma", endedAt: 9 }));
    expect(sendMail(store, cfg, alpha, { ...base, to: "gamma" }, 1).toRole).toBe("gamma");
  });

  test("answers need evidence and keep it", () => {
    const m = sendMail(store, cfg, alpha, { ...base, kind: "answer", evidence: "a.ts:3" }, 1);
    expect(m.evidence).toBe("a.ts:3");
  });

  test("the owner (no session) has the same per-minute cap", () => {
    const owner: Caller = { role: "owner", sessionId: null };
    for (let i = 0; i < MAX_MAIL_PER_MINUTE; i++) sendMail(store, cfg, owner, base, 1_000 + i);
    expect(refusal(() => sendMail(store, cfg, owner, base, 2_000))).toBe("rate-limited");
  });

  test("a thread that looks like a secret is refused", () => {
    expect(
      refusal(() =>
        sendMail(store, cfg, alpha, { ...base, thread: "sk-abcdefghijklmnopqrstuvwxyz012345" }, 1),
      ),
    ).toBe("looks-secret");
  });

  test("one session may send at most the per-minute cap", () => {
    for (let i = 0; i < MAX_MAIL_PER_MINUTE; i++) sendMail(store, cfg, alpha, base, 1_000 + i);
    expect(refusal(() => sendMail(store, cfg, alpha, base, 2_000))).toBe("rate-limited");
    expect(refusal(() => sendMail(store, cfg, alpha, base, 61_001))).toBe("sent");
  });
});

describe("answerAndNotify", () => {
  test("stores the answer and mails it to the owning role in one step", () => {
    const d = store.mail().createDecision({
      role: "alpha",
      question: "Ship?",
      options: [],
      recommendation: null,
      fromSession: "s-alpha",
      createdAt: 1,
    });
    const r = answerAndNotify(store, cfg, d.id, "yes", "tray", 10);
    expect(r.result).toBe("answered");
    const inbox = store.mail().inbox("alpha");
    expect(inbox).toHaveLength(1);
    expect(inbox[0]).toMatchObject({
      fromRole: "owner",
      toRole: "alpha",
      kind: "decision",
      thread: d.id,
      body: "D-0001 answered: yes (recorded via tray, outside Claude Code; not proof of who sent it)",
    });
  });

  test("a conflicting or repeated answer sends no mail", () => {
    store.mail().createDecision({
      role: "alpha",
      question: "Ship?",
      options: [],
      recommendation: null,
      fromSession: null,
      createdAt: 1,
    });
    answerAndNotify(store, cfg, "D-0001", "yes", "tray", 10);
    expect(answerAndNotify(store, cfg, "D-0001", "yes", "cli", 11).result).toBe("same");
    expect(answerAndNotify(store, cfg, "D-0001", "no", "cli", 12).result).toBe("conflict");
    expect(store.mail().inbox("alpha")).toHaveLength(1);
    const changed = answerAndNotify(store, cfg, "D-0001", "no", "cli", 13, { replace: true });
    expect(changed.result).toBe("replaced");
    const bodies = store
      .mail()
      .inbox("alpha")
      .map((m) => m.body);
    expect(bodies).toEqual([
      "D-0001 answered: yes (recorded via tray, outside Claude Code; not proof of who sent it)",
      "D-0001 answer changed: no (was: yes; recorded via cli, outside Claude Code; not proof of who sent it)",
    ]);
  });

  test("an answer with a secret is refused before anything is stored", () => {
    store.mail().createDecision({
      role: "alpha",
      question: "Key?",
      options: [],
      recommendation: null,
      fromSession: null,
      createdAt: 1,
    });
    expect(() =>
      answerAndNotify(store, cfg, "D-0001", "ghp_abcdefghijklmnopqrstuvwxyz0123456789", "cli", 2),
    ).toThrow(MailRefused);
    expect(store.mail().getDecision("D-0001")?.state).toBe("open");
  });
});

describe("mailUpkeep", () => {
  test("expired mail and unacked mail each tell the sender once, from the hub", () => {
    store.upsertSession(session("s-beta", { profile: "home", name: "beta" }));
    const ms = store.mail();
    const send = (body: string, expiresAt: number) =>
      ms.insert({
        thread: "T-9",
        fromRole: "alpha",
        fromSession: "s-alpha",
        toRole: "beta",
        kind: "task",
        body,
        evidence: null,
        createdAt: 0,
        expiresAt,
      });
    send("old", 10);
    send("slow", 10_000_000);
    ms.claim("beta", "s-beta", 20);
    ms.insert({
      thread: "T-9",
      fromRole: "alpha",
      fromSession: "s-alpha",
      toRole: "beta",
      kind: "task",
      body: "late",
      evidence: null,
      createdAt: 30,
      expiresAt: 40,
    });
    const now = 20 + cfg.ackTimeoutMs;
    const r = mailUpkeep(store, cfg, now);
    expect(r).toEqual({ expired: 1, requeued: 0, flagged: 2, closed: 0 });
    const notes = ms.inbox("alpha");
    expect(notes.map((m) => [m.fromRole, m.kind, m.thread])).toEqual([
      ["hub", "fyi", "T-9"],
      ["hub", "fyi", "T-9"],
      ["hub", "fyi", "T-9"],
    ]);
    expect(notes.map((m) => m.body)).toEqual([
      "M-000003 to beta expired: no session of beta took it before 1970-01-01T00:00:00.040Z.",
      "M-000001 to beta has no ack 30 min after it was delivered.",
      "M-000002 to beta has no ack 30 min after it was delivered.",
    ]);
    expect(mailUpkeep(store, cfg, now + 1)).toEqual({
      expired: 0,
      requeued: 0,
      flagged: 0,
      closed: 0,
    });
  });

  test("mail of an ended session goes back to the queue", () => {
    store.upsertSession(session("s-beta", { profile: "home", name: "beta" }));
    const ms = store.mail();
    ms.insert({
      thread: "T",
      fromRole: "alpha",
      fromSession: null,
      toRole: "beta",
      kind: "fyi",
      body: "b",
      evidence: null,
      createdAt: 0,
      expiresAt: 9e12,
    });
    ms.claim("beta", "s-beta", 1);
    store.markEnded("s-beta", 2);
    expect(mailUpkeep(store, cfg, 3).requeued).toBe(1);
    expect(ms.get(1)?.state).toBe("queued");
  });

  test("notices about the hub's own mail are never sent", () => {
    const ms = store.mail();
    ms.insert({
      thread: "T",
      fromRole: "hub",
      fromSession: null,
      toRole: "alpha",
      kind: "fyi",
      body: "note",
      evidence: null,
      createdAt: 0,
      expiresAt: 5,
    });
    expect(mailUpkeep(store, cfg, 10)).toEqual({ expired: 1, requeued: 0, flagged: 0, closed: 0 });
    expect(ms.inbox("hub", { all: true })).toEqual([]);
  });

  test("an open decision older than the limit is closed and its role told once (PII-03)", () => {
    const ms = store.mail();
    const base = {
      role: "alpha",
      question: "q",
      options: [],
      recommendation: null,
      fromSession: null,
    };
    ms.createDecision({ ...base, createdAt: 0 });
    ms.createDecision({ ...base, createdAt: 0 });
    ms.createDecision({ ...base, createdAt: 1 });
    ms.answerDecision("D-0002", "yes", "cli", 5);
    const now = OPEN_DECISION_MAX_MS + 1;
    expect(mailUpkeep(store, cfg, now)).toEqual({ expired: 0, requeued: 0, flagged: 0, closed: 1 });
    expect(ms.getDecision("D-0001")?.state).toBe("closed");
    expect(ms.getDecision("D-0001")?.closedAt).toBe(now);
    expect(ms.getDecision("D-0002")?.state).toBe("answered");
    expect(ms.getDecision("D-0003")?.state).toBe("open");
    const notes = ms.inbox("alpha");
    expect(notes.map((m) => [m.fromRole, m.kind, m.thread, m.body])).toEqual([
      ["hub", "fyi", "D-0001", "D-0001 closed: no answer within 30 days."],
    ]);
    expect(mailUpkeep(store, cfg, now).closed).toBe(0);
  });
});
