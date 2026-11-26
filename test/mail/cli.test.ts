import { Database } from "bun:sqlite";
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { type MailCliDeps, runMailCli } from "../../src/mail/cli";
import { type SessionRow, Store } from "../../src/store/db";
import { MIGRATIONS } from "../../src/store/schema";

let dir: string;
let dbPath: string;
let clock: number;
let files: Map<string, string>;
let roles: string | null;
const opened: Store[] = [];

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

function open(): Store {
  const s = Store.open(dbPath);
  opened.push(s);
  return s;
}

interface Run {
  code: number;
  out: string[];
  err: string[];
}

function deps(
  env: Record<string, string | undefined>,
  over: Partial<MailCliDeps> = {},
): MailCliDeps & { outLines: string[]; errLines: string[] } {
  const outLines: string[] = [];
  const errLines: string[] = [];
  return {
    outLines,
    errLines,
    openStore: () => Store.open(dbPath, { create: false }),
    readRoles: () => roles,
    env,
    now: () => clock,
    out: (l) => outLines.push(l),
    err: (l) => errLines.push(l),
    readInput: async (p) => {
      const t = files.get(p);
      if (t === undefined) throw new Error(`ENOENT: ${p}`);
      return t;
    },
    sleep: async (ms) => {
      clock += ms;
    },
    callerWaitMs: 0,
    ...over,
  };
}

async function run(
  cmd: string,
  args: string[],
  env: Record<string, string | undefined> = {},
  over: Partial<MailCliDeps> = {},
): Promise<Run> {
  const d = deps(env, over);
  const code = await runMailCli(cmd, args, d);
  return { code, out: d.outLines, err: d.errLines };
}

const ALPHA = { CLAUDE_CODE_SESSION_ID: "s-alpha", CLAUDECODE: "1" };
const BETA = { CLAUDE_CODE_SESSION_ID: "s-beta", CLAUDECODE: "1" };

function json(line: string | undefined): Record<string, unknown> {
  return JSON.parse(line ?? "null") as Record<string, unknown>;
}

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "hub-mailcli-"));
  dbPath = join(dir, "hub.db");
  clock = 1_000_000;
  files = new Map([["body.txt", "please run the tests\nand report"]]);
  roles = JSON.stringify({
    roles: [
      { role: "alpha", profile: "work" },
      { role: "beta", profile: "home" },
    ],
  });
  const s = open();
  s.upsertSession(session("s-alpha", { name: "alpha" }));
  s.upsertSession(session("s-beta", { name: "beta", profile: "home" }));
});

afterEach(() => {
  for (const s of opened.splice(0)) s.close();
  rmSync(dir, { recursive: true, force: true });
});

const SEND = [
  "send",
  "--to",
  "beta",
  "--thread",
  "T-1",
  "--kind",
  "task",
  "--body-file",
  "body.txt",
];

describe("hub mail send", () => {
  test("queues the file's text and says who will get it", async () => {
    const r = await run("mail", SEND, ALPHA);
    expect(r.code).toBe(0);
    expect(r.out).toEqual(["M-000001 queued for beta; live session: beta"]);
    const m = open().mail().get(1);
    expect(m?.body).toBe("please run the tests\nand report");
    expect(m?.fromRole).toBe("alpha");
  });

  test("says when no session of the role is live", async () => {
    open().markEnded("s-beta", 5);
    const r = await run("mail", SEND, ALPHA);
    expect(r.out[0]).toBe(
      "M-000001 queued for beta; no live session of beta: it waits until one starts (expires 1970-01-04T00:16:40.000Z)",
    );
  });

  test("reads the body from stdin with --body-file - and strips a byte order mark", async () => {
    files.set("-", "\uFEFFfrom stdin");
    const args = [...SEND.slice(0, -1), "-"];
    expect((await run("mail", args, ALPHA)).code).toBe(0);
    expect(open().mail().get(1)?.body).toBe("from stdin");
  });

  test("--json prints the stored row", async () => {
    const r = await run("mail", [...SEND, "--json"], ALPHA);
    expect(json(r.out[0])).toMatchObject({ id: "M-000001", state: "queued", to: "beta" });
  });

  test("a refusal prints its reason and exits 1", async () => {
    const r = await run("mail", ["send", "--to", "zeta", ...SEND.slice(3)], ALPHA);
    expect(r.code).toBe(1);
    expect(r.err[0]).toStartWith("hub mail: refused: unknown-role");
  });

  test("a missing body file exits 1 without storing anything", async () => {
    const r = await run("mail", [...SEND.slice(0, -1), "nope.txt"], ALPHA);
    expect(r.code).toBe(1);
    expect(open().mail().get(1)).toBeNull();
  });

  test("missing or unknown flags are usage errors", async () => {
    expect((await run("mail", ["send", "--to", "beta"], ALPHA)).code).toBe(2);
    expect((await run("mail", [...SEND, "--bogus"], ALPHA)).code).toBe(2);
    expect((await run("mail", ["nonsense"], ALPHA)).code).toBe(2);
    expect((await run("mail", [], ALPHA)).code).toBe(2);
  });
});

describe("hub mail follow", () => {
  test("prints each new message as one JSON line and ends after --max-minutes", async () => {
    await run("mail", SEND, ALPHA);
    let inserted = false;
    const r = await run("mail", ["follow", "--max-minutes", "1"], BETA, {
      sleep: async (ms) => {
        clock += ms;
        if (!inserted && clock > 1_010_000) {
          inserted = true;
          files.set("b2.txt", "second");
          await run("mail", [...SEND.slice(0, -1), "b2.txt"], ALPHA);
        }
      },
    });
    expect(r.code).toBe(0);
    expect(r.out).toHaveLength(3);
    const first = json(r.out[0]);
    expect(first).toMatchObject({
      type: "mail",
      id: "M-000001",
      thread: "T-1",
      from: "alpha",
      to: "beta",
      kind: "task",
      redelivered: false,
      body: "please run the tests\nand report",
    });
    expect(String(first.notice)).toContain("hub mail ack M-000001");
    expect(json(r.out[1])).toMatchObject({ id: "M-000002", body: "second" });
    expect(json(r.out[2])).toMatchObject({ type: "end", reason: "max-minutes" });
    expect(open().mail().get(1)?.state).toBe("delivered");
    expect(open().mail().get(1)?.deliveredSession).toBe("s-beta");
  });

  test("on start it prints again what this session got but did not ack", async () => {
    await run("mail", SEND, ALPHA);
    open().mail().claim("beta", "s-beta", clock);
    const r = await run("mail", ["follow", "--max-minutes", "1"], BETA);
    expect(json(r.out[0])).toMatchObject({ id: "M-000001", redelivered: true });
  });

  test("stops when its session ends", async () => {
    let n = 0;
    const r = await run("mail", ["follow"], BETA, {
      sleep: async (ms) => {
        clock += ms;
        n += 1;
        if (n === 3) open().markEnded("s-beta", clock);
      },
    });
    expect(r.code).toBe(0);
    expect(json(r.out.at(-1))).toMatchObject({ type: "end", reason: "session-ended" });
  });

  test("JSON lines escape C1 and bidi control characters", async () => {
    files.set("evil.txt", "a\u009b31mb\u202ec");
    await run("mail", [...SEND.slice(0, -1), "evil.txt"], ALPHA);
    const r = await run("mail", ["follow", "--max-minutes", "1"], BETA);
    expect(r.out[0]).toContain("\\u009b");
    expect(r.out[0]).toContain("\\u202e");
    expect(r.out[0]).not.toContain("\u009b");
    expect(json(r.out[0]).body).toBe("a\u009b31mb\u202ec");
  });

  test("mail from the owner or the hub says its sender is not verified", async () => {
    files.set("o.txt", "from a terminal");
    await run(
      "mail",
      ["send", "--to", "beta", "--thread", "T", "--kind", "fyi", "--body-file", "o.txt"],
      {},
    );
    const r = await run("mail", ["follow", "--max-minutes", "1"], BETA);
    const n = String(json(r.out[0]).notice);
    expect(n).toContain("outside Claude Code");
    expect(n).toContain("not proof");
    expect(n).toContain("never approves");
  });

  test("refuses the owner and another role", async () => {
    expect((await run("mail", ["follow"], {})).code).toBe(1);
    const r = await run("mail", ["follow", "--role", "alpha"], BETA);
    expect(r.code).toBe(1);
    expect(r.err[0]).toContain("not-your-role");
  });
});

describe("hub mail inbox, ack, done, sent, show", () => {
  test("inbox delivers and lists open mail; ack and done move it on", async () => {
    await run("mail", SEND, ALPHA);
    const inbox = await run("mail", ["inbox", "--json"], BETA);
    expect(inbox.code).toBe(0);
    expect(json(inbox.out[0])).toMatchObject({ id: "M-000001", state: "delivered" });
    expect((await run("mail", ["ack", "M-000001"], ALPHA)).code).toBe(1);
    const ack = await run("mail", ["ack", "M-1"], BETA);
    expect(ack.out).toEqual(["M-000001 acked"]);
    expect((await run("mail", ["inbox", "--json"], BETA)).out).toEqual([]);
    expect((await run("mail", ["done", "M-000001"], ALPHA)).out).toEqual(["M-000001 done"]);
    const sent = await run("mail", ["sent", "--json"], ALPHA);
    expect(json(sent.out[0])).toMatchObject({ id: "M-000001", state: "done" });
  });

  test("inbox without --json prints readable, terminal-safe text", async () => {
    files.set("x.txt", "line one\u001b[2Jline two");
    await run("mail", [...SEND.slice(0, -1), "x.txt"], ALPHA);
    const r = await run("mail", ["inbox"], BETA);
    const text = r.out.join("\n");
    expect(text).toContain("M-000001  task  from alpha  thread T-1");
    expect(text).toContain("line oneline two");
    expect(text).not.toContain("\u001b");
  });

  test("show prints one message to its sender or recipient only", async () => {
    await run("mail", SEND, ALPHA);
    open().upsertSession(session("s-gamma", { name: "gamma" }));
    expect((await run("mail", ["show", "M-000001", "--json"], ALPHA)).code).toBe(0);
    const r = await run("mail", ["show", "1"], { CLAUDE_CODE_SESSION_ID: "s-gamma" });
    expect(r.code).toBe(1);
  });

  test("the owner reads owner mail from a terminal outside Claude Code", async () => {
    files.set("o.txt", "for the owner");
    await run(
      "mail",
      ["send", "--to", "owner", "--thread", "T", "--kind", "fyi", "--body-file", "o.txt"],
      ALPHA,
    );
    const r = await run("mail", ["inbox", "--json"], {});
    expect(json(r.out[0])).toMatchObject({ to: "owner", body: "for the owner" });
    expect((await run("mail", ["ack", "M-000001"], {})).out).toEqual(["M-000001 acked"]);
  });

  test("a bad id is reported and the exit code is 1", async () => {
    const r = await run("mail", ["ack", "M-x", "M-000009"], BETA);
    expect(r.code).toBe(1);
    expect(r.err).toHaveLength(2);
  });
});

describe("hub ask, answer, decisions", () => {
  test("ask opens a decision; the owner's answer is mailed to the role", async () => {
    files.set("q.txt", "Ship the release?");
    const ask = await run(
      "ask",
      ["--question-file", "q.txt", "--option", "yes", "--option", "no", "--recommend", "yes"],
      ALPHA,
    );
    expect(ask.code).toBe(0);
    expect(ask.out).toEqual(["D-0001 open for alpha"]);
    const ans = await run("answer", ["D-0001", "--text", "yes"], {});
    expect(ans.code).toBe(0);
    expect(ans.out[0]).toBe("D-0001 answered; the answer was mailed to alpha");
    const inbox = await run("mail", ["inbox", "--json"], ALPHA);
    expect(json(inbox.out[0])).toMatchObject({
      from: "owner",
      kind: "decision",
      thread: "D-0001",
      body: "D-0001 answered: yes (recorded via cli, outside Claude Code; not proof of who sent it)",
    });
  });

  test("a session cannot answer, and a second different answer is a conflict", async () => {
    files.set("q.txt", "Ship?");
    await run("ask", ["--question-file", "q.txt"], ALPHA);
    const inSession = await run("answer", ["D-0001", "--text", "yes"], BETA);
    expect(inSession.code).toBe(1);
    expect(inSession.err[0]).toContain("not-owner");
    await run("answer", ["D-0001", "--text", "yes"], {});
    expect((await run("answer", ["D-0001", "--text", "yes"], {})).out[0]).toBe(
      "D-0001 already has this answer; nothing changed",
    );
    const clash = await run("answer", ["D-0001", "--text", "no"], {});
    expect(clash.code).toBe(1);
    expect(clash.err[0]).toContain("conflict");
    expect((await run("answer", ["D-0001", "--text", "no", "--replace"], {})).code).toBe(0);
  });

  test("decisions lists them; the owning role closes its own", async () => {
    files.set("q.txt", "Ship?");
    await run("ask", ["--question-file", "q.txt", "--option", "yes"], ALPHA);
    const list = await run("decisions", ["--json"], {});
    expect(json(list.out[0])).toMatchObject({ id: "D-0001", role: "alpha", state: "open" });
    const human = await run("decisions", [], {});
    expect(human.out[0]).toBe("D-0001  open  alpha  Ship?");
    expect((await run("decisions", ["close", "D-0001"], BETA)).code).toBe(1);
    expect((await run("decisions", ["close", "D-0001"], ALPHA)).out).toEqual(["D-0001 closed"]);
  });

  test("ask outside a session is refused", async () => {
    files.set("q.txt", "Ship?");
    expect((await run("ask", ["--question-file", "q.txt"], {})).code).toBe(1);
  });
});

describe("hub roles", () => {
  test("lists roles with their live sessions and queued mail", async () => {
    open().markEnded("s-beta", 5);
    await run("mail", SEND, ALPHA);
    const r = await run("roles", ["--json"], {});
    const rows = r.out.map((l) => json(l));
    expect(rows).toContainEqual({
      role: "beta",
      profile: "home",
      sessions: ["beta"],
      live: [],
      queued: 1,
    });
    expect(rows).toContainEqual({
      role: "alpha",
      profile: "work",
      sessions: ["alpha"],
      live: ["alpha"],
      queued: 0,
    });
    const human = (await run("roles", [], {})).out.join("\n");
    expect(human).toContain("beta: 1 queued, no live session (start one named beta)");
  });
});

describe("review fixes", () => {
  test("text output cleans every stored field, not only the body", async () => {
    const s = open();
    s.mail().insert({
      thread: "T\u001b]0;x\u0007-1",
      fromRole: "al\u001b[31mpha",
      fromSession: null,
      toRole: "beta",
      kind: "task",
      body: "b",
      evidence: null,
      createdAt: clock,
      expiresAt: clock + 1e9,
    });
    s.mail().insert({
      thread: "T",
      fromRole: "alpha",
      fromSession: null,
      toRole: "ro\u009ble",
      kind: "task",
      body: "b",
      evidence: null,
      createdAt: clock,
      expiresAt: clock + 1e9,
    });
    s.mail().createDecision({
      role: "alpha",
      question: "q",
      options: [],
      recommendation: null,
      fromSession: null,
      createdAt: 1,
    });
    s.mail().answerDecision("D-0001", "ok", "tr\u001b[2Jay", 2);
    s.upsertSession(session("s-bad", { name: "ba\u001b[2Jd name" }));
    const outs = [
      ...(await run("mail", ["inbox"], BETA)).out,
      ...(await run("decisions", [], {})).out,
      ...(await run("roles", [], {})).out,
    ];
    const errs = (await run("mail", ["inbox"], { CLAUDE_CODE_SESSION_ID: "s-bad" })).err;
    for (const line of [...outs, ...errs]) {
      const unsafe = [...line].filter((c) => {
        const n = c.charCodeAt(0);
        return n === 0x1b || n === 0x07 || (n >= 0x80 && n <= 0x9f);
      });
      expect(unsafe).toEqual([]);
    }
    expect(errs[0]).toContain("no-role");
  });

  test("a DEL character still arrives byte for byte in JSON output", async () => {
    files.set("del.txt", "a\u007fb");
    await run("mail", [...SEND.slice(0, -1), "del.txt"], ALPHA);
    const r = await run("mail", ["inbox", "--json"], BETA);
    expect(json(r.out[0]).body).toBe("a\u007fb");
  });

  test("a newline in a stored name never splits an output line", async () => {
    open().upsertSession(session("s-nl", { name: "bad\nname here" }));
    const r = await run("mail", ["inbox"], { CLAUDE_CODE_SESSION_ID: "s-nl" });
    expect(r.err).toHaveLength(1);
    expect(r.err[0]).not.toContain("\n");
  });

  test("mail taken again after its session ended is marked redelivered", async () => {
    await run("mail", SEND, ALPHA);
    open().mail().claim("beta", "s-beta", clock);
    const s = open();
    s.markEnded("s-beta", clock);
    s.mail().requeueEnded();
    s.upsertSession(session("s-beta-new", { name: "beta", profile: "home" }));
    const r = await run("mail", ["follow", "--max-minutes", "1"], {
      CLAUDE_CODE_SESSION_ID: "s-beta-new",
    });
    expect(json(r.out[0])).toMatchObject({ id: "M-000001", deliveries: 2, redelivered: true });
  });

  test("follow never reprints mail another session of the same role took", async () => {
    await run("mail", SEND, ALPHA);
    open().upsertSession(session("s-beta-2", { name: "beta", profile: "home" }));
    open().mail().claim("beta", "s-beta-2", clock);
    const r = await run("mail", ["follow", "--max-minutes", "1"], BETA);
    expect(r.out.map((l) => json(l).type)).toEqual(["end"]);
  });

  test("an index older than this build is refused, not upgraded", async () => {
    const old = join(dir, "old.db");
    const raw = new Database(old, { create: true, strict: true });
    raw.run("CREATE TABLE schema_version (version INTEGER NOT NULL)");
    for (const m of MIGRATIONS.slice(0, 5)) raw.run(m);
    raw.run("INSERT INTO schema_version (version) VALUES (1), (2), (3), (4), (5)");
    raw.close();
    const r = await run(
      "roles",
      [],
      {},
      {
        openStore: () => Store.open(old, { create: false, migrate: false }),
      },
    );
    expect(r.code).toBe(1);
    expect(r.err[0]).toContain("index schema v5");
    const check = new Database(old, { readonly: true });
    const v = check.query("SELECT MAX(version) AS v FROM schema_version").get() as { v: number };
    check.close();
    expect(v.v).toBe(5);
  });
});

describe("help", () => {
  test("--help prints the mail usage and exits 0, without opening the index", async () => {
    for (const [cmd, args] of [
      ["mail", ["--help"]],
      ["ask", ["-h"]],
      ["roles", ["--help"]],
    ] as const) {
      const r = await run(cmd, [...args], {}, { openStore: () => null });
      expect(r.code).toBe(0);
      expect(r.out.join(" ")).toContain("hub mail send --to <role>");
    }
  });
});

describe("setup errors", () => {
  test("a missing hub.db or a bad roles.json exits 1", async () => {
    roles = "{";
    expect((await run("roles", [], {})).code).toBe(1);
    roles = null;
    const r = await run("roles", [], {}, { openStore: () => null });
    expect(r.code).toBe(1);
    expect(r.err[0]).toContain("hub.db");
  });
});

describe("end to end", () => {
  test("cross-profile mail is delivered and acked in under 5 s, with real timers", async () => {
    const real = { now: () => Date.now(), sleep: (ms: number) => Bun.sleep(ms) };
    const t0 = Date.now();
    const lines: string[] = [];
    const follower = runMailCli("mail", ["follow", "--max-minutes", "0.05"], {
      ...deps(BETA),
      ...real,
      out: (l) => lines.push(l),
    });
    await Bun.sleep(100);
    const sent = await run("mail", SEND, ALPHA, real);
    expect(sent.code).toBe(0);
    while (lines.length === 0 && Date.now() - t0 < 5_000) await Bun.sleep(20);
    const got = json(lines[0]);
    expect(got.id).toBe("M-000001");
    const ack = await run("mail", ["ack", String(got.id)], BETA, real);
    expect(ack.code).toBe(0);
    expect(Date.now() - t0).toBeLessThan(5_000);
    expect(open().mail().get(1)?.state).toBe("acked");
    expect(await follower).toBe(0);
  });
});
