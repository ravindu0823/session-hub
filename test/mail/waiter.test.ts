import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { LEASE_MS, sqliteWaiterState, type WaiterState } from "../../src/mail/waiter";

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
  test("one waiter per session: a second claim, from another connection, gets none", () => {
    const a = state(10);
    const b = state(11);
    const t = a.claim("s-1", 1_000, 0);
    expect(t).not.toBeNull();
    expect(b.claim("s-1", 1_000, 1)).toBeNull();
    expect(b.claim("s-2", 1_000, 1)).not.toBeNull();
    a.release("s-1", t ?? "");
    expect(b.claim("s-1", 1_000, 2)).not.toBeNull();
  });

  test("a losing claim extends the holder's deadline, never shortens it", () => {
    const a = state();
    const t = a.claim("s-1", 1_000, 0) ?? "";
    expect(state(11).claim("s-1", 5_000, 10)).toBeNull();
    expect(a.renew("s-1", t, 20)).toBe(5_000);
    expect(state(12).claim("s-1", 3_000, 30)).toBeNull();
    expect(a.renew("s-1", t, 40)).toBe(5_000);
  });

  test("a lease with no heartbeat for the lease time is taken over; the old holder then stops", () => {
    const a = state(10);
    const b = state(11);
    const t = a.claim("s-1", 999_999, 0) ?? "";
    expect(a.renew("s-1", t, LEASE_MS - 1)).toBe(999_999);
    expect(b.claim("s-1", 999_999, 2 * LEASE_MS - 1)).toBeNull(); // heartbeat kept it alive
    const t2 = b.claim("s-1", 999_999, 3 * LEASE_MS);
    expect(t2).not.toBeNull();
    expect(a.renew("s-1", t, 3 * LEASE_MS + 1)).toBeNull();
    a.release("s-1", t); // not its lease any more: no effect
    expect(b.renew("s-1", t2 ?? "", 3 * LEASE_MS + 2)).toBe(999_999);
  });

  test("two takeovers of the same stale lease leave exactly one holder", () => {
    state(9).claim("s-1", 1, 0);
    const now = 10 * LEASE_MS;
    const tokens = [state(10).claim("s-1", 2, now), state(11).claim("s-1", 2, now)];
    expect(tokens.filter((t) => t !== null)).toHaveLength(1);
  });

  test("a session id that is not a plain name is refused", () => {
    expect(() => state().claim("../x", 1, 0)).toThrow();
    expect(existsSync(join(dir, "x"))).toBe(false);
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
