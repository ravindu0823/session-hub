import { Database } from "bun:sqlite";
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { type MailCliDeps, runMailCli } from "../../src/mail/cli";
import { type SessionRow, Store } from "../../src/store/db";

let dir: string;
let dbPath: string;
let clock: number;
let input: string | null;
let roles: string | null;
let offFile: string | null;
const opened: Store[] = [];

function session(id: string, name: string): SessionRow {
  return {
    sessionId: id,
    profile: "work",
    name,
    cwd: "C:/x",
    pid: 100,
    kind: "interactive",
    status: "busy",
    startedAt: 1000,
    updatedAt: 2000,
    endedAt: null,
    transcriptPath: `C:/x/${id}.jsonl`,
    permissionMode: null,
  };
}

const POLICY = {
  denyTools: ["mcp__ops*", "mcp__vault__exec"],
  denyAgents: ["ops-*", "general-purpose", "Explore"],
  writeOnlyUnder: ["C:\\work\\notes"],
  reason: "coordinator only: delegate to the owning session",
};

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "hub-guard-"));
  dbPath = join(dir, "hub.db");
  clock = 1_000_000;
  input = null;
  offFile = null;
  roles = JSON.stringify({
    roles: [{ role: "lead", profile: "work", guard: POLICY }, { role: "dev" }],
  });
  const s = Store.open(dbPath);
  opened.push(s);
  s.upsertSession(session("s-lead", "lead"));
  s.upsertSession(session("s-dev", "dev"));
});

afterEach(() => {
  for (const s of opened.splice(0)) s.close();
  rmSync(dir, { recursive: true, force: true });
});

async function guard(
  args: string[] = [],
  env: Record<string, string | undefined> = {},
  over: Partial<MailCliDeps> = {},
) {
  const out: string[] = [];
  const err: string[] = [];
  const d: MailCliDeps = {
    openStore: () => Store.open(dbPath, { create: false, migrate: false }),
    readRoles: () => roles,
    env,
    now: () => clock,
    out: (l) => out.push(l),
    err: (l) => err.push(l),
    readInput: async () => {
      if (input === null) throw new Error("no stdin");
      return input;
    },
    sleep: async () => {},
    exists: () => true,
    readGuardOff: () => offFile,
    writeGuardOff: (t) => {
      offFile = t;
    },
    ...over,
  };
  const code = await runMailCli("guard", args, d);
  return { code, out, err };
}

function hook(sid: string, tool: string, toolInput: Record<string, unknown> = {}): void {
  input = JSON.stringify({
    session_id: sid,
    hook_event_name: "PreToolUse",
    tool_name: tool,
    tool_input: { ...toolInput, content: "SECRET FILE CONTENT" },
  });
}

describe("hub guard as a PreToolUse hook", () => {
  test("blocks a denied tool for the guarded role: exit 2 and the reason on stderr", async () => {
    hook("s-lead", "mcp__ops-live__deploy");
    const r = await guard();
    expect(r.code).toBe(2);
    expect(r.out).toEqual([]);
    expect(r.err).toHaveLength(1);
    expect(r.err[0]).toContain("blocked for role lead");
    expect(r.err[0]).toContain("coordinator only: delegate to the owning session");
    expect(r.err[0]).toContain("ask the owner; do not work around this block");
    expect(r.err[0]).not.toContain("SECRET");
  });

  test("blocks denied agent types and writes outside the allowed folders", async () => {
    hook("s-lead", "Agent", { subagent_type: "Explore", prompt: "go" });
    expect((await guard()).code).toBe(2);
    hook("s-lead", "Agent", { prompt: "go" });
    expect((await guard()).code).toBe(2);
    hook("s-lead", "Write", { file_path: "C:\\repo\\a.ts" });
    expect((await guard()).code).toBe(2);
    hook("s-lead", "NotebookEdit", { notebook_path: "C:\\repo\\n.ipynb" });
    expect((await guard()).code).toBe(2);
  });

  test("allows what the policy does not deny", async () => {
    const pass = { code: 0, out: [], err: [] };
    for (const [tool, ti] of [
      ["Read", { file_path: "C:\\repo\\a.ts" }],
      ["Bash", { command: "gh pr view 1" }],
      ["mcp__vault__list_handles", {}],
      ["Agent", { subagent_type: "reviewer" }],
      ["Write", { file_path: "C:\\work\\notes\\register.md" }],
    ] as const) {
      hook("s-lead", tool, ti);
      expect(await guard()).toEqual(pass);
    }
  });

  test("never touches a role without a policy", async () => {
    hook("s-dev", "mcp__ops-live__deploy");
    expect(await guard()).toEqual({ code: 0, out: [], err: [] });
  });

  test("no policy anywhere: the index is never opened", async () => {
    roles = JSON.stringify({ roles: [{ role: "lead" }] });
    hook("s-lead", "mcp__ops-live__deploy");
    let opens = 0;
    const r = await guard(
      [],
      {},
      {
        openStore: () => {
          opens++;
          return null;
        },
      },
    );
    expect(r.code).toBe(0);
    expect(opens).toBe(0);
  });

  test("fails open: bad input, unknown session, no index, a bad roles.json", async () => {
    input = "not json";
    expect((await guard()).code).toBe(0);
    input = null;
    expect((await guard()).code).toBe(0);
    hook("bad id; x", "mcp__ops-live__deploy");
    expect((await guard()).code).toBe(0);
    hook("s-unknown", "mcp__ops-live__deploy");
    expect((await guard()).code).toBe(0);
    hook("s-lead", "mcp__ops-live__deploy");
    expect((await guard([], {}, { openStore: () => null })).code).toBe(0);
    roles = "{";
    const r = await guard();
    expect(r).toEqual({ code: 0, out: [], err: ["hub guard: skipped (RolesConfigError)"] });
  });

  test("an owner pause lets calls through until it ends", async () => {
    offFile = JSON.stringify({ lead: clock + 60_000 });
    hook("s-lead", "mcp__ops-live__deploy");
    expect((await guard()).code).toBe(0);
    clock += 60_000;
    expect((await guard()).code).toBe(2);
    offFile = "garbage";
    expect((await guard()).code).toBe(2);
  });

  test("the hook writes nothing to the index", async () => {
    const raw = new Database(dbPath, { readonly: true });
    try {
      const version = () =>
        (raw.query("PRAGMA data_version").get() as { data_version: number }).data_version;
      const before = version();
      hook("s-lead", "mcp__ops-live__deploy");
      expect((await guard()).code).toBe(2);
      expect(version()).toBe(before);
    } finally {
      raw.close();
    }
  });
});

describe("hub guard: review fixes", () => {
  test("each tool's own path field decides; a stray second field does not", async () => {
    hook("s-lead", "NotebookEdit", {
      notebook_path: "C:\\repo\\x.ipynb",
      file_path: "C:\\work\\notes\\a.md",
    });
    expect((await guard()).code).toBe(2);
    hook("s-lead", "Edit", {
      file_path: "C:\\repo\\a.ts",
      notebook_path: "C:\\work\\notes\\a.ipynb",
    });
    expect((await guard()).code).toBe(2);
    hook("s-lead", "Edit", {
      file_path: `C:\\repo\\${"a".repeat(3000)}`,
      notebook_path: "C:\\work\\notes\\a",
    });
    expect((await guard()).code).toBe(2);
  });

  test("a hostile pause file never pauses for good and never breaks status", async () => {
    hook("s-lead", "mcp__ops-live__deploy");
    for (const text of ["[1e15]", "null", '{"lead":1e20}', '{"lead":"x"}', "{"]) {
      offFile = text;
      expect((await guard()).code).toBe(2);
      const s = await guard(["status"]);
      expect(s.code).toBe(0);
      expect(s.out[0]).toStartWith("lead: ON");
    }
  });

  test("a pause for a role without a policy is refused", async () => {
    const r = await guard(["off", "--role", "dev", "--minutes", "5"]);
    expect(r.code).toBe(1);
    expect(r.err[0]).toContain("has no guard policy");
    expect(offFile).toBeNull();
  });

  test("a second session of a role is guarded only when the role's sessions pattern names it", async () => {
    const s = Store.open(dbPath);
    opened.push(s);
    s.upsertSession(session("s-lead2", "lead-2"));
    hook("s-lead2", "mcp__ops-live__deploy");
    expect((await guard()).code).toBe(0); // role "lead-2": no policy
    roles = JSON.stringify({
      roles: [{ role: "lead", sessions: ["lead", "lead-*"], guard: POLICY }],
    });
    expect((await guard()).code).toBe(2);
  });
});

describe("hub guard status, off and on", () => {
  test("status lists policies and pauses", async () => {
    let r = await guard(["status"]);
    expect(r.out).toEqual(["lead: ON (2 tools, 3 agent types, writes only under C:\\work\\notes)"]);
    offFile = JSON.stringify({ lead: clock + 60_000 });
    r = await guard(["status"]);
    expect(r.out[0]).toStartWith("lead: PAUSED until 1970-01-01 00:17 UTC");
    roles = JSON.stringify({ roles: [] });
    expect((await guard(["status"])).out).toEqual(["no role has a guard policy"]);
  });

  test("off and on, from outside Claude Code", async () => {
    const off = await guard(["off", "--role", "lead", "--minutes", "30"]);
    expect(off.code).toBe(0);
    expect(JSON.parse(offFile ?? "{}")).toEqual({ lead: clock + 30 * 60_000 });
    const on = await guard(["on", "--role", "lead"]);
    expect(on).toEqual({ code: 0, out: ["lead: guard ON"], err: [] });
    expect(JSON.parse(offFile ?? "{}")).toEqual({});
  });

  test("ended pauses are dropped when the file is written", async () => {
    offFile = JSON.stringify({ old: clock - 1, other: clock + 5 });
    await guard(["off", "--role", "lead", "--minutes", "1"]);
    expect(Object.keys(JSON.parse(offFile ?? "{}")).sort()).toEqual(["lead", "other"]);
  });

  test("refused inside a Claude Code session", async () => {
    for (const env of [{ CLAUDECODE: "1" }, { CLAUDE_CODE_SESSION_ID: "s-lead" }]) {
      const r = await guard(["off", "--role", "lead", "--minutes", "30"], env);
      expect(r.code).toBe(1);
      expect(r.err[0]).toContain("refused");
    }
    expect(offFile).toBeNull();
  });

  test("usage errors", async () => {
    for (const args of [
      ["off", "--role", "lead"],
      ["off", "--role", "lead", "--minutes", "0"],
      ["off", "--role", "lead", "--minutes", "481"],
      ["off", "--role", "Lead!", "--minutes", "5"],
      ["on"],
      ["x"],
      ["status", "x"],
    ]) {
      expect((await guard(args)).code).toBe(1); // never 2: that would block every hooked call
    }
    expect((await guard(["--help"])).out[0]).toContain("Per-role tool policy");
  });
});
