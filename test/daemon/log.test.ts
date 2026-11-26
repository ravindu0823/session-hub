import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, readFileSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createLogger, errFields } from "../../src/daemon/log.ts";

let dir: string;
beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "hub-log-"));
});
afterEach(() => rmSync(dir, { recursive: true, force: true }));

function lines(file: string): Record<string, unknown>[] {
  return readFileSync(file, "utf8")
    .split("\n")
    .filter((l) => l !== "")
    .map((l) => JSON.parse(l) as Record<string, unknown>);
}

describe("createLogger", () => {
  test("writes JSON lines with ts, level, event and fields", () => {
    const log = createLogger(join(dir, "run"));
    log.info("start", { n: 1, ok: true, id: "abc" });
    log.warn("slow", { ms: 5 });
    log.error("boom");
    const got = lines(join(dir, "run", "daemon.log"));
    expect(got.map((l) => [l.level, l.event])).toEqual([
      ["info", "start"],
      ["warn", "slow"],
      ["error", "boom"],
    ]);
    expect(got[0]).toMatchObject({ n: 1, ok: true, id: "abc" });
    expect(typeof got[0]?.ts).toBe("string");
  });

  test("another file name in the same folder (relay.log)", () => {
    const log = createLogger(dir, undefined, "relay.log");
    log.info("relay", { sendId: 1 });
    expect(existsSync(join(dir, "daemon.log"))).toBe(false);
    expect(lines(join(dir, "relay.log"))[0]).toMatchObject({ event: "relay", sendId: 1 });
  });

  test("fields cannot override ts, level or event", () => {
    const log = createLogger(dir);
    log.info("real", { ts: "x", level: "error", event: "fake" });
    const got = lines(join(dir, "daemon.log"));
    expect(got[0]?.level).toBe("info");
    expect(got[0]?.event).toBe("real");
    expect(got[0]?.ts).not.toBe("x");
  });

  test("string fields are truncated to 200 chars", () => {
    const log = createLogger(dir);
    log.info("long", { s: "a".repeat(500) });
    const got = lines(join(dir, "daemon.log"));
    expect(String(got[0]?.s).length).toBe(200);
  });

  test("rotates at the size limit and keeps 3 files", () => {
    const log = createLogger(dir, 1000);
    for (let i = 0; i < 100; i++) log.info("fill", { i, pad: "x".repeat(100) });
    expect(existsSync(join(dir, "daemon.log"))).toBe(true);
    expect(existsSync(join(dir, "daemon.log.1"))).toBe(true);
    expect(existsSync(join(dir, "daemon.log.2"))).toBe(true);
    expect(existsSync(join(dir, "daemon.log.3"))).toBe(false);
    for (const f of ["daemon.log", "daemon.log.1", "daemon.log.2"]) {
      expect(readFileSync(join(dir, f)).length).toBeLessThanOrEqual(1000);
    }
    const last = lines(join(dir, "daemon.log")).at(-1);
    expect(last?.i).toBe(99);
  });
});

describe("errFields", () => {
  test("name and truncated message only", () => {
    const e = new TypeError("m".repeat(300));
    const f = errFields(e);
    expect(f.err).toBe("TypeError");
    expect((f.msg as string).length).toBe(200);
    expect(errFields("plain")).toEqual({ err: "Error", msg: "plain" });
  });

  test("a file-system error keeps its code, never its path (PII-04)", () => {
    let caught: unknown;
    try {
      statSync(join(dir, "Jane Doe", "secret-project", "a b.jsonl"));
    } catch (e) {
      caught = e;
    }
    const f = errFields(caught);
    expect(f).toEqual({ err: "Error", code: "ENOENT" });
    for (const leak of ["Jane", "Doe", "secret-project", "a b.jsonl"]) {
      expect(JSON.stringify(f)).not.toContain(leak);
    }
  });

  test("absolute paths are removed from any error text", () => {
    const cases: [string, string][] = [
      ["open C:\\Users\\jane\\x.json failed", "open <path> failed"],
      ["stat 'D:/work/my proj/x.jsonl'", "stat <path>"],
      ['read "C:\\a b\\c"', "read <path>"],
      ["share \\\\host\\share\\f is gone", "share <path> is gone"],
      ["no /home/jane/.claude/x here", "no <path> here"],
      ["database is locked", "database is locked"],
      ["UNIQUE constraint failed: mail.id", "UNIQUE constraint failed: mail.id"],
    ];
    for (const [text, want] of cases) expect(errFields(new Error(text)).msg).toBe(want);
    expect(errFields("bad C:\\x\\y").msg).toBe("bad <path>");
  });

  test("canary: a path in a logged error never reaches daemon.log", () => {
    const log = createLogger(dir);
    try {
      statSync(join(dir, "canary-folder-7f3", "t.jsonl"));
    } catch (e) {
      log.warn("scan_error", { session: "s1", ...errFields(e) });
    }
    const text = readFileSync(join(dir, "daemon.log"), "utf8");
    expect(text).toContain('"code":"ENOENT"');
    expect(text).not.toContain("canary-folder-7f3");
  });
});
