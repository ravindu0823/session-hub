import {
  closeSync,
  mkdirSync,
  openSync,
  readFileSync,
  rmSync,
  writeFileSync,
  writeSync,
} from "node:fs";
import { join } from "node:path";

/**
 * What `hub mail wait` keeps between runs, per session: a lock so at most one waiter runs, and the
 * highest mail id it already rang for, so the same mail never rings twice.
 */
export interface WaiterState {
  /** True when this process now holds the session's waiter lock. */
  claim(sessionId: string): boolean;
  /** Drops the lock if this process holds it. */
  release(sessionId: string): void;
  lastRung(sessionId: string): number | null;
  setRung(sessionId: string, id: number): void;
}

/** Session ids as Claude Code writes them; anything else is never used in a path. */
const SESSION_RE = /^[A-Za-z0-9-]{1,100}$/;

/** Files `<sessionId>.lock` (holder pid) and `<sessionId>.rung` in `dir`, created on first use. */
export function fileWaiterState(
  dir: string,
  proc: { pid: number; alive(pid: number): boolean },
): WaiterState {
  const lockPath = (s: string) => join(dir, `${s}.lock`);
  const holder = (s: string): number | null => {
    try {
      const n = Number(readFileSync(lockPath(s), "utf8"));
      return Number.isSafeInteger(n) && n > 0 ? n : null;
    } catch {
      return null;
    }
  };
  const create = (s: string): boolean => {
    try {
      const fd = openSync(lockPath(s), "wx");
      try {
        writeSync(fd, String(proc.pid));
      } finally {
        closeSync(fd);
      }
      return true;
    } catch {
      return false;
    }
  };
  return {
    claim(s) {
      if (!SESSION_RE.test(s)) return false;
      mkdirSync(dir, { recursive: true });
      if (create(s)) return true;
      const pid = holder(s);
      if (pid !== null && pid !== proc.pid && proc.alive(pid)) return false;
      // A stale lock (its holder is gone, or it is unreadable): replace it once. A racing waiter
      // that also saw it stale loses the exclusive create.
      rmSync(lockPath(s), { force: true });
      return create(s);
    },
    release(s) {
      if (SESSION_RE.test(s) && holder(s) === proc.pid) rmSync(lockPath(s), { force: true });
    },
    lastRung(s) {
      if (!SESSION_RE.test(s)) return null;
      try {
        const n = Number(readFileSync(join(dir, `${s}.rung`), "utf8"));
        return Number.isSafeInteger(n) && n >= 0 ? n : null;
      } catch {
        return null;
      }
    },
    setRung(s, id) {
      if (!SESSION_RE.test(s)) return;
      mkdirSync(dir, { recursive: true });
      writeFileSync(join(dir, `${s}.rung`), String(id));
    },
  };
}

/** Whether a process exists: signal 0 checks without sending (EPERM means it exists). */
export function processAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (e) {
    return (e as { code?: string }).code === "EPERM";
  }
}
