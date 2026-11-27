import { Database } from "bun:sqlite";
import { mkdirSync, statSync } from "node:fs";
import { dirname } from "node:path";

/** A mail as the doorbell keys it: requeued mail (one more delivery) counts as new. */
export interface MailKey {
  id: number;
  deliveries: number;
}

/**
 * What `hub mail wait` keeps between runs, per session: a lease so at most one waiter runs, the
 * time the waiter may wait until (a newer claim extends it), and the mail keys already rung or
 * named, so the same mail never rings twice. Every method is one transaction.
 */
export interface WaiterState {
  /**
   * Takes the session's waiter lease until `until`, returning its token. The newest claim always
   * wins: an older waiter (alive, or killed at its hook timeout before it could let go) sees its
   * token gone at its next renew and exits, so the session is always covered by the newest hook.
   */
  claim(sessionId: string, until: number, now: number): string;
  /** Heartbeat: the time to wait until while `token` still holds the lease, else null. */
  renew(sessionId: string, token: string, now: number): number | null;
  release(sessionId: string, token: string): void;
  /**
   * The keys not rung for this session yet, which are recorded as rung. On the session's first
   * call nothing is new: what is queued then was named at session start or by a prompt.
   */
  takeNew(sessionId: string, keys: readonly MailKey[], now: number): MailKey[];
  /** Records keys as rung (the hook named them), without ringing. */
  markRung(sessionId: string, keys: readonly MailKey[], now: number): void;
  close(): void;
}

/** Sessions not seen for this long are forgotten. */
const KEEP_MS = 14 * 24 * 3600_000;

/** `PRAGMA user_version` of waiters.db; raise it with any change to the tables below. */
const SCHEMA_VERSION = 1;
const SCHEMA = `
CREATE TABLE IF NOT EXISTS waiter (
  session_id TEXT PRIMARY KEY,
  token TEXT NOT NULL,
  pid INTEGER NOT NULL,
  seen_at INTEGER NOT NULL,
  until_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS session (
  session_id TEXT PRIMARY KEY,
  seen_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS rung (
  session_id TEXT NOT NULL,
  mail_id INTEGER NOT NULL,
  deliveries INTEGER NOT NULL,
  PRIMARY KEY (session_id, mail_id, deliveries)
);`;

/** Session ids as Claude Code writes them; anything else is refused. */
const SESSION_RE = /^[A-Za-z0-9-]{1,100}$/;

/** The state in one SQLite file (its own, never hub.db), created on first use. */
export function sqliteWaiterState(path: string, pid: number): WaiterState {
  mkdirSync(dirname(path), { recursive: true });
  const db = new Database(path, { create: true, readwrite: true, strict: true });
  try {
    db.run("PRAGMA busy_timeout = 2000");
    db.run("PRAGMA journal_mode = WAL");
    const v = (db.query("PRAGMA user_version").get() as { user_version: number }).user_version;
    if (v > SCHEMA_VERSION) throw new Error(`waiters.db version ${v} is newer than this build`);
    db.run(SCHEMA);
    if (v < SCHEMA_VERSION) db.run(`PRAGMA user_version = ${SCHEMA_VERSION}`);
  } catch (e) {
    db.close();
    throw e;
  }
  const check = (s: string) => {
    if (!SESSION_RE.test(s)) throw new Error("bad session id");
  };
  const touch = (s: string, now: number) => {
    const known = db.query("SELECT 1 FROM session WHERE session_id = ?").get(s) !== null;
    db.query(
      "INSERT INTO session (session_id, seen_at) VALUES (?, ?) ON CONFLICT(session_id) DO UPDATE SET seen_at = excluded.seen_at",
    ).run(s, now);
    return known;
  };
  const record = (s: string, keys: readonly MailKey[]) => {
    const ins = db.query(
      "INSERT OR IGNORE INTO rung (session_id, mail_id, deliveries) VALUES (?, ?, ?)",
    );
    for (const k of keys) ins.run(s, k.id, k.deliveries);
  };
  return {
    claim: db.transaction((s: string, until: number, now: number): string => {
      check(s);
      // Forget old sessions here, the one place every waiter passes.
      const old = now - KEEP_MS;
      db.query(
        "DELETE FROM rung WHERE session_id IN (SELECT session_id FROM session WHERE seen_at < ?)",
      ).run(old);
      db.query("DELETE FROM session WHERE seen_at < ?").run(old);
      db.query("DELETE FROM waiter WHERE seen_at < ?").run(old);
      const token = crypto.randomUUID();
      db.query(
        `INSERT INTO waiter (session_id, token, pid, seen_at, until_at) VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(session_id) DO UPDATE SET token = excluded.token, pid = excluded.pid,
           seen_at = excluded.seen_at, until_at = excluded.until_at`,
      ).run(s, token, pid, now, until);
      return token;
    }).immediate,
    renew: db.transaction((s: string, token: string, now: number): number | null => {
      check(s);
      const row = db
        .query("SELECT token, until_at AS until FROM waiter WHERE session_id = ?")
        .get(s) as { token: string; until: number } | null;
      if (row === null || row.token !== token) return null;
      db.query("UPDATE waiter SET seen_at = ? WHERE session_id = ?").run(now, s);
      return row.until;
    }).immediate,
    release(s, token) {
      check(s);
      db.query("DELETE FROM waiter WHERE session_id = ? AND token = ?").run(s, token);
    },
    takeNew: db.transaction((s: string, keys: readonly MailKey[], now: number): MailKey[] => {
      check(s);
      const known = touch(s, now);
      const has = db.query(
        "SELECT 1 FROM rung WHERE session_id = ? AND mail_id = ? AND deliveries = ?",
      );
      const fresh = known ? keys.filter((k) => has.get(s, k.id, k.deliveries) === null) : [];
      record(s, keys);
      return fresh;
    }).immediate,
    markRung: db.transaction((s: string, keys: readonly MailKey[], now: number): void => {
      check(s);
      touch(s, now);
      record(s, keys);
    }).immediate,
    close() {
      db.close();
    },
  };
}

/** True when the file exists and changed within the last `ms` (a waiter stop signal). */
export function recentFile(path: string, ms: number, now = Date.now()): boolean {
  try {
    // A copy keeps its source's modified time on Windows; its creation time is the copy's.
    const s = statSync(path);
    return now - Math.max(s.birthtimeMs, s.mtimeMs) < ms;
  } catch {
    return false;
  }
}
