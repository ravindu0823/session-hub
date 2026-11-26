import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Store } from "../../src/store/db";

// Several processes on one hub.db, released together: every step is one conditional statement or an
// IMMEDIATE transaction, so nothing is applied twice.

let dir: string;
let dbPath: string;
let goFile: string;
const WORKER = join(import.meta.dir, "worker.ts");

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "hub-mailconc-"));
  dbPath = join(dir, "hub.db");
  goFile = join(dir, "go");
});

afterEach(() => {
  rmSync(dir, { recursive: true, force: true });
});

async function race<T>(args: [string, string][]): Promise<T[]> {
  const procs = args.map(([mode, arg]) =>
    Bun.spawn([process.execPath, WORKER, dbPath, goFile, mode, arg], {
      stdout: "pipe",
      stderr: "pipe",
    }),
  );
  await Bun.sleep(300);
  writeFileSync(goFile, "");
  return Promise.all(
    procs.map(async (p) => {
      const [out, err, code] = await Promise.all([
        new Response(p.stdout).text(),
        new Response(p.stderr).text(),
        p.exited,
      ]);
      if (code !== 0) throw new Error(`worker exited ${code}: ${err}`);
      return JSON.parse(out) as T;
    }),
  );
}

describe("several processes on one index", () => {
  test("every queued mail is claimed exactly once", async () => {
    const s = Store.open(dbPath);
    for (let i = 0; i < 200; i++) {
      s.mail().insert({
        thread: "T",
        fromRole: "alpha",
        fromSession: null,
        toRole: "beta",
        kind: "fyi",
        body: `m${i}`,
        evidence: null,
        createdAt: 1,
        expiresAt: 9e12,
      });
    }
    s.close();
    const got = await race<number[]>([
      ["claim", "w1"],
      ["claim", "w2"],
      ["claim", "w3"],
      ["claim", "w4"],
    ]);
    const all = got.flat().sort((a, b) => a - b);
    expect(all).toHaveLength(200);
    expect(new Set(all).size).toBe(200);
  }, 30_000);

  test("two answers at once: one is stored, the other is a conflict, one mail is sent", async () => {
    const s = Store.open(dbPath);
    s.mail().createDecision({
      role: "alpha",
      question: "q",
      options: [],
      recommendation: null,
      fromSession: null,
      createdAt: 1,
    });
    s.close();
    const results = await race<string>([
      ["answer", "yes"],
      ["answer", "no"],
    ]);
    expect(results.sort()).toEqual(["answered", "conflict"]);
    const check = Store.open(dbPath);
    expect(check.mail().inbox("alpha")).toHaveLength(1);
    check.close();
  }, 30_000);

  test("decisions asked at once get distinct ids", async () => {
    Store.open(dbPath).close();
    const ids = await race<string>([
      ["ask", "a"],
      ["ask", "b"],
      ["ask", "c"],
      ["ask", "d"],
    ]);
    expect(new Set(ids).size).toBe(4);
  }, 30_000);
});
