import { Database } from "bun:sqlite";
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { copyFileSync, existsSync, mkdtempSync, rmSync, utimesSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { recentFile, sqliteWaiterState, type WaiterState } from "../../src/mail/waiter";

let dir: string;
let path: string;
const open: WaiterState[] = [];
function state(pid = 10): WaiterState {
  const s = sqliteWaiterState(path, pid);
  open.push(s);
  return s;
}
beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "hub-waiter-"));
  path = join(dir, "w", "waiters.db");
});
afterEach(() => {
  for (const s of open.splice(0)) s.close();
  rmSync(dir, { recursive: true, force: true });
});

const k = (id: number, deliveries = 0) => ({ id, deliveries });

describe("waiter lease", () => {
  test("the newest claim wins; the older holder sees its token gone at its next renew", () => {
    const a = state(10);
    const b = state(11);
    const t1 = a.claim("s-1", 1_000, 0);
    expect(a.renew("s-1", t1, 1)).toBe(1_000);
    const t2 = b.claim("s-1", 5_000, 2);
    expect(t2).not.toBe(t1);
    expect(a.renew("s-1", t1, 3)).toBeNull();
    expect(b.renew("s-1", t2, 4)).toBe(5_000);
    a.release("s-1", t1); // not its lease any more: no effect
    expect(b.renew("s-1", t2, 5)).toBe(5_000);
    b.release("s-1", t2);
    expect(b.renew("s-1", t2, 6)).toBeNull();
  });

  test("a holder that was killed (never released) does not block the next claim", () => {
    state(9).claim("s-1", 1, 0);
    const t = state(10).claim("s-1", 99, 1);
    expect(state(11).renew("s-1", t, 2)).toBe(99);
  });

  test("sessions are leased apart", () => {
    const a = state();
    const t1 = a.claim("s-1", 10, 0);
    a.claim("s-2", 20, 0);
    expect(a.renew("s-1", t1, 1)).toBe(10);
  });

  test("a session id that is not a plain name is refused", () => {
    expect(() => state().claim("../x", 1, 0)).toThrow();
    expect(existsSync(join(dir, "x"))).toBe(false);
  });

  test("the file carries a schema version; a newer file is refused", () => {
    state().close();
    const raw = new Database(path);
    expect((raw.query("PRAGMA user_version").get() as { user_version: number }).user_version).toBe(
      1,
    );
    raw.run("PRAGMA user_version = 2");
    raw.close();
    expect(() => sqliteWaiterState(path, 1)).toThrow("newer");
  });
});

describe("recentFile", () => {
  test("a fresh copy of an old file is recent; a missing file is not", () => {
    const old = join(dir, "old.txt");
    writeFileSync(old, "x");
    utimesSync(old, new Date(2020, 0, 1), new Date(2020, 0, 1));
    const copy = join(dir, "old.txt.new");
    copyFileSync(old, copy);
    expect(recentFile(copy, 120_000)).toBe(true);
    expect(recentFile(copy, 120_000, Date.now() + 200_000)).toBe(false);
    expect(recentFile(join(dir, "none"), 120_000)).toBe(false);
  });
});

describe("rung mail", () => {
  test("the first look sets a baseline; later only new keys come back, once", () => {
    const a = state();
    expect(a.takeNew("s-1", [k(1), k(2)], 0)).toEqual([]);
    expect(a.takeNew("s-1", [k(1), k(2), k(3)], 1)).toEqual([k(3)]);
    expect(a.takeNew("s-1", [k(1), k(2), k(3)], 2)).toEqual([]);
    expect(state(11).takeNew("s-1", [k(3), k(4)], 3)).toEqual([k(4)]);
  });

  test("requeued mail (one more delivery) is new again", () => {
    const a = state();
    a.takeNew("s-1", [], 0);
    expect(a.takeNew("s-1", [k(5)], 1)).toEqual([k(5)]);
    expect(a.takeNew("s-1", [k(5, 1)], 2)).toEqual([k(5, 1)]);
  });

  test("keys the hook named never ring; the hook also makes the session known", () => {
    const a = state();
    a.markRung("s-1", [k(1)], 0);
    expect(a.takeNew("s-1", [k(1), k(2)], 1)).toEqual([k(2)]);
  });

  test("sessions are per id", () => {
    const a = state();
    a.takeNew("s-1", [], 0);
    expect(a.takeNew("s-2", [k(1)], 1)).toEqual([]);
    expect(a.takeNew("s-1", [k(1)], 2)).toEqual([k(1)]);
  });

  test("a session not seen for 14 days is forgotten at the next claim", () => {
    const a = state();
    a.takeNew("s-1", [k(1)], 0);
    a.claim("s-2", 1, 15 * 24 * 3600_000);
    expect(a.takeNew("s-1", [k(1), k(2)], 15 * 24 * 3600_000 + 1)).toEqual([]);
  });
});
