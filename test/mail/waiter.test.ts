import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileWaiterState } from "../../src/mail/waiter";

let dir: string;
beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "hub-waiter-"));
});
afterEach(() => {
  rmSync(dir, { recursive: true, force: true });
});

describe("file waiter state", () => {
  test("one waiter per session: a second claim fails while the first holder is alive", () => {
    const a = fileWaiterState(join(dir, "w"), { pid: 10, alive: () => true });
    const b = fileWaiterState(join(dir, "w"), { pid: 11, alive: () => true });
    expect(a.claim("s-1")).toBe(true);
    expect(b.claim("s-1")).toBe(false);
    expect(b.claim("s-2")).toBe(true);
    a.release("s-1");
    expect(b.claim("s-1")).toBe(true);
  });

  test("a lock left by a dead process is taken over", () => {
    const a = fileWaiterState(join(dir, "w"), { pid: 10, alive: () => true });
    expect(a.claim("s-1")).toBe(true);
    const b = fileWaiterState(join(dir, "w"), { pid: 11, alive: (pid) => pid !== 10 });
    expect(b.claim("s-1")).toBe(true);
    expect(readFileSync(join(dir, "w", "s-1.lock"), "utf8")).toBe("11");
  });

  test("a lock with unreadable content is taken over", () => {
    const a = fileWaiterState(join(dir, "w"), { pid: 10, alive: () => true });
    expect(a.claim("s-0")).toBe(true);
    writeFileSync(join(dir, "w", "s-1.lock"), "not a pid");
    expect(a.claim("s-1")).toBe(true);
  });

  test("release removes only its own lock", () => {
    const a = fileWaiterState(join(dir, "w"), { pid: 10, alive: () => false });
    const b = fileWaiterState(join(dir, "w"), { pid: 11, alive: () => false });
    expect(a.claim("s-1")).toBe(true);
    expect(b.claim("s-1")).toBe(true);
    a.release("s-1");
    expect(existsSync(join(dir, "w", "s-1.lock"))).toBe(true);
    b.release("s-1");
    expect(existsSync(join(dir, "w", "s-1.lock"))).toBe(false);
  });

  test("the last rung id is kept per session; a missing or bad value is null", () => {
    const a = fileWaiterState(join(dir, "w"), { pid: 10, alive: () => true });
    expect(a.lastRung("s-1")).toBeNull();
    a.setRung("s-1", 42);
    expect(a.lastRung("s-1")).toBe(42);
    expect(a.lastRung("s-2")).toBeNull();
    writeFileSync(join(dir, "w", "s-2.rung"), "-5");
    expect(a.lastRung("s-2")).toBeNull();
    writeFileSync(join(dir, "w", "s-2.rung"), "x");
    expect(a.lastRung("s-2")).toBeNull();
  });

  test("a session id that is not a plain name is refused, never used as a path", () => {
    const a = fileWaiterState(join(dir, "w"), { pid: 10, alive: () => true });
    expect(a.claim("../x")).toBe(false);
    expect(a.lastRung("../x")).toBeNull();
    a.setRung("../x", 1);
    expect(existsSync(join(dir, "x.rung"))).toBe(false);
  });
});
