import { Database } from "bun:sqlite";
import { RETENTION_MS } from "../config";
import type { TailState } from "../ingest/tail";
import type { Status, WaitingItem } from "../status/extract";
import { MIGRATIONS } from "./schema";

export interface SessionRow {
  sessionId: string;
  profile: string;
  name: string;
  cwd: string;
  pid: number;
  kind: string;
  status: "busy" | "idle" | "waiting" | "unknown";
  startedAt: number;
  updatedAt: number;
  endedAt: number | null;
  transcriptPath: string;
  permissionMode: string | null;
}

export interface MessageRow {
  uuid: string;
  sessionId: string;
  ts: string;
  role: "owner" | "peer" | "assistant";
  fromName: string | null;
  text: string;
}

export type SendState = "pending" | "held" | "delivered" | "expired" | "failed";

export interface SendRow {
  id?: number;
  sessionId: string;
  profile: string;
  name: string;
  sha256: string;
  createdAt: number;
  state: SendState;
  detail: string | null;
}

export interface StoredStatus extends Status {
  replyUuid: string;
  replyTs: string;
}

export interface LatestReply {
  replyUuid: string;
  replyTs: string;
  hasStatus: boolean;
}

/**
 * SessionRow's own `status` (busy/idle/...) is exposed as `state` because the controller ruling
 * gives `status` the parsed four-part status.
 */
export interface SessionView extends Omit<SessionRow, "status"> {
  state: SessionRow["status"];
  status: StoredStatus | null;
  latestReply: LatestReply | null;
  waitingCount: number;
}

export interface SearchHit {
  sessionId: string;
  uuid: string;
  ts: string;
  role: MessageRow["role"];
  snippet: string;
}

type Row = Record<string, unknown>;

function ftsQuery(q: string): string {
  return q
    .split(/\s+/)
    .filter((t) => t.length > 0)
    .map((t) => `"${t.replaceAll('"', '""')}"`)
    .join(" ");
}

function toSession(r: Row): SessionRow {
  return {
    sessionId: r.session_id as string,
    profile: r.profile as string,
    name: r.name as string,
    cwd: r.cwd as string,
    pid: r.pid as number,
    kind: r.kind as string,
    status: r.status as SessionRow["status"],
    startedAt: r.started_at as number,
    updatedAt: r.updated_at as number,
    endedAt: (r.ended_at as number | null) ?? null,
    transcriptPath: r.transcript_path as string,
    permissionMode: (r.permission_mode as string | null) ?? null,
  };
}

function toMessage(r: Row): MessageRow {
  return {
    uuid: r.uuid as string,
    sessionId: r.session_id as string,
    ts: r.ts as string,
    role: r.role as MessageRow["role"],
    fromName: (r.from_name as string | null) ?? null,
    text: r.text as string,
  };
}

function toSend(r: Row): SendRow {
  return {
    id: r.id as number,
    sessionId: r.session_id as string,
    profile: r.profile as string,
    name: r.name as string,
    sha256: r.sha256 as string,
    createdAt: r.created_at as number,
    state: r.state as SendState,
    detail: (r.detail as string | null) ?? null,
  };
}

/** Messages deleted per write transaction by purge. */
const PURGE_BATCH = 500;

export class Store {
  private constructor(private readonly db: Database) {}

  /**
   * Opens the index file, creating it when missing unless `create` is false (then a missing file
   * throws). The caller ensures the directory exists.
   */
  static open(path: string, opts: { create?: boolean } = {}): Store {
    const create = opts.create ?? true;
    const db = new Database(path, { create, readwrite: true, strict: true });
    db.run("PRAGMA busy_timeout = 5000");
    const mode = db.query("PRAGMA journal_mode = WAL").get() as { journal_mode: string } | null;
    if (mode?.journal_mode !== "wal") {
      db.close();
      throw new Error("sqlite WAL mode unavailable");
    }
    db.run("PRAGMA synchronous = NORMAL");
    db.run("PRAGMA foreign_keys = ON");
    // Freed pages are zeroed, so deleted message text cannot be read back from the file (PII-03).
    db.run("PRAGMA secure_delete = ON");
    try {
      Store.migrate(db);
    } catch (e) {
      db.close();
      throw e;
    }
    return new Store(db);
  }

  private static migrate(db: Database): void {
    db.run("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)");
    const apply = db.transaction(() => {
      const row = db.query("SELECT MAX(version) AS v FROM schema_version").get() as {
        v: number | null;
      };
      let current = row.v ?? 0;
      if (current > MIGRATIONS.length) {
        throw new Error(
          `index schema v${current} is newer than this build (v${MIGRATIONS.length})`,
        );
      }
      while (current < MIGRATIONS.length) {
        db.run(MIGRATIONS[current] as string);
        current += 1;
        db.query("INSERT INTO schema_version (version) VALUES (?)").run(current);
      }
    });
    apply.immediate();
  }

  close(): void {
    this.db.close();
  }

  /** Runs fn in one write transaction; rolls back if it throws. */
  tx<T>(fn: () => T): T {
    return this.db.transaction(fn).immediate();
  }

  /** VACUUM, then a truncating checkpoint so the WAL file keeps no copy of purged pages. */
  vacuum(): void {
    this.db.run("VACUUM");
    this.db.run("PRAGMA wal_checkpoint(TRUNCATE)");
  }

  // ---- sessions ----

  upsertSession(s: SessionRow): void {
    this.db
      .query(
        `INSERT INTO sessions (session_id, profile, name, cwd, pid, kind, status, started_at,
           updated_at, ended_at, transcript_path, permission_mode)
         VALUES ($sessionId, $profile, $name, $cwd, $pid, $kind, $status, $startedAt,
           $updatedAt, $endedAt, $transcriptPath, $permissionMode)
         ON CONFLICT(session_id) DO UPDATE SET
           profile=excluded.profile, name=excluded.name, cwd=excluded.cwd, pid=excluded.pid,
           kind=excluded.kind, status=excluded.status, started_at=excluded.started_at,
           updated_at=excluded.updated_at, ended_at=excluded.ended_at,
           transcript_path=excluded.transcript_path, permission_mode=excluded.permission_mode`,
      )
      .run({ ...s });
  }

  markEnded(sessionId: string, at: number): void {
    this.db
      .query("UPDATE sessions SET ended_at = ?, updated_at = ? WHERE session_id = ?")
      .run(at, at, sessionId);
  }

  /** One session row by id, or null. */
  getSession(sessionId: string): SessionRow | null {
    const r = this.db
      .query("SELECT * FROM sessions WHERE session_id = ?")
      .get(sessionId) as Row | null;
    return r ? toSession(r) : null;
  }

  listSessions(): SessionView[] {
    const rows = this.db
      .query(
        `SELECT s.*, st.progress_pct, st.phase, st.remaining_json, st.waiting_json,
                st.recommendations_json, st.reply_uuid AS st_reply_uuid, st.reply_ts AS st_reply_ts,
                lr.reply_uuid AS lr_reply_uuid, lr.reply_ts AS lr_reply_ts, lr.has_status
           FROM sessions s
           LEFT JOIN status st ON st.session_id = s.session_id
           LEFT JOIN latest_reply lr ON lr.session_id = s.session_id
          ORDER BY (s.ended_at IS NOT NULL), s.profile, s.name, s.session_id`,
      )
      .all() as Row[];
    return rows.map((r) => {
      const waiting = r.waiting_json ? (JSON.parse(r.waiting_json as string) as WaitingItem[]) : [];
      const status: StoredStatus | null =
        r.st_reply_uuid == null
          ? null
          : {
              progressPct: (r.progress_pct as number | null) ?? null,
              phase: (r.phase as string | null) ?? null,
              remaining: JSON.parse(r.remaining_json as string) as string[],
              waiting,
              recommendations: JSON.parse(r.recommendations_json as string) as string[],
              replyUuid: r.st_reply_uuid as string,
              replyTs: r.st_reply_ts as string,
            };
      const latestReply: LatestReply | null =
        r.lr_reply_uuid == null
          ? null
          : {
              replyUuid: r.lr_reply_uuid as string,
              replyTs: r.lr_reply_ts as string,
              hasStatus: r.has_status === 1,
            };
      const { status: state, ...rest } = toSession(r);
      return { ...rest, state, status, latestReply, waitingCount: waiting.length };
    });
  }

  // ---- tails ----

  getTail(path: string): TailState | null {
    const r = this.db.query("SELECT * FROM tails WHERE path = ?").get(path) as Row | null;
    if (!r) return null;
    return {
      path: r.path as string,
      offset: r.offset as number,
      size: r.size as number,
      firstUuid: (r.first_uuid as string | null) ?? null,
    };
  }

  saveTail(t: TailState): void {
    this.db
      .query(
        `INSERT INTO tails (path, offset, size, first_uuid) VALUES (?, ?, ?, ?)
         ON CONFLICT(path) DO UPDATE SET offset=excluded.offset, size=excluded.size,
           first_uuid=excluded.first_uuid`,
      )
      .run(t.path, t.offset, t.size, t.firstUuid);
  }

  // ---- messages ----

  /** Text must already be masked by the caller. Duplicate uuids are ignored. */
  addMessages(sessionId: string, rows: MessageRow[]): void {
    const ins = this.db.query(
      `INSERT OR IGNORE INTO messages (uuid, session_id, ts, role, from_name, text)
       VALUES (?, ?, ?, ?, ?, ?)`,
    );
    this.db
      .transaction(() => {
        for (const m of rows) ins.run(m.uuid, sessionId, m.ts, m.role, m.fromName, m.text);
      })
      .immediate();
  }

  /**
   * One page, oldest first, before the cursor. Pages run in `ts DESC, id DESC` order. With only
   * `beforeTs`, rows sharing that ts are skipped (kept for old callers). With `beforeUuid` too, the
   * cursor is that row: rows of the same ts that come before it in the page order are included, so
   * no row is skipped or repeated across pages. An unknown `beforeUuid` acts like `beforeTs` alone.
   */
  history(sessionId: string, limit: number, beforeTs?: string, beforeUuid?: string): MessageRow[] {
    let rows: Row[];
    if (beforeTs === undefined) {
      rows = this.db
        .query("SELECT * FROM messages WHERE session_id = ? ORDER BY ts DESC, id DESC LIMIT ?")
        .all(sessionId, limit) as Row[];
    } else if (beforeUuid === undefined) {
      rows = this.db
        .query(
          `SELECT * FROM messages WHERE session_id = ? AND ts < ?
            ORDER BY ts DESC, id DESC LIMIT ?`,
        )
        .all(sessionId, beforeTs, limit) as Row[];
    } else {
      rows = this.db
        .query(
          `SELECT * FROM messages WHERE session_id = $sid
              AND (ts < $ts OR (ts = $ts AND id < (
                SELECT id FROM messages WHERE session_id = $sid AND uuid = $uuid)))
            ORDER BY ts DESC, id DESC LIMIT $limit`,
        )
        .all({ sid: sessionId, ts: beforeTs, uuid: beforeUuid, limit }) as Row[];
    }
    return rows.reverse().map(toMessage);
  }

  search(q: string, limit: number): SearchHit[] {
    const match = ftsQuery(q);
    if (match === "") return [];
    const rows = this.db
      .query(
        `SELECT m.session_id, m.uuid, m.ts, m.role,
                snippet(messages_fts, 0, '[', ']', '...', 12) AS snippet
           FROM messages_fts
           JOIN messages m ON m.id = messages_fts.rowid
          WHERE messages_fts MATCH ?
          ORDER BY m.ts DESC, m.id DESC
          LIMIT ?`,
      )
      .all(match, limit) as Row[];
    return rows.map((r) => ({
      sessionId: r.session_id as string,
      uuid: r.uuid as string,
      ts: r.ts as string,
      role: r.role as MessageRow["role"],
      snippet: r.snippet as string,
    }));
  }

  // ---- status ----

  setStatus(sessionId: string, st: Status, replyUuid: string, replyTs: string): void {
    this.db
      .query(
        `INSERT INTO status (session_id, progress_pct, phase, remaining_json, waiting_json,
           recommendations_json, reply_uuid, reply_ts) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(session_id) DO UPDATE SET progress_pct=excluded.progress_pct,
           phase=excluded.phase, remaining_json=excluded.remaining_json,
           waiting_json=excluded.waiting_json, recommendations_json=excluded.recommendations_json,
           reply_uuid=excluded.reply_uuid, reply_ts=excluded.reply_ts`,
      )
      .run(
        sessionId,
        st.progressPct,
        st.phase,
        JSON.stringify(st.remaining),
        JSON.stringify(st.waiting),
        JSON.stringify(st.recommendations),
        replyUuid,
        replyTs,
      );
  }

  setLatestReply(sessionId: string, replyUuid: string, replyTs: string, hasStatus: boolean): void {
    this.db
      .query(
        `INSERT INTO latest_reply (session_id, reply_uuid, reply_ts, has_status) VALUES (?, ?, ?, ?)
         ON CONFLICT(session_id) DO UPDATE SET reply_uuid=excluded.reply_uuid,
           reply_ts=excluded.reply_ts, has_status=excluded.has_status`,
      )
      .run(sessionId, replyUuid, replyTs, hasStatus ? 1 : 0);
  }

  // ---- retention ----

  /**
   * Deletes sessions ended before now - RETENTION_MS (cascading), their tails, old sends, and
   * messages older than the window in every session, live ones included (PII-03). Message
   * timestamps are ISO UTC strings, so they compare as text.
   */
  purge(now: number): number {
    const cutoff = now - RETENTION_MS;
    const cutoffIso = new Date(cutoff).toISOString();
    let deleted = 0;
    // Old messages go in batches, each its own short write transaction: the first purge after an
    // upgrade can hold days of a live session's backlog, and one huge delete (with secure-delete)
    // would hold the write lock past other processes' 5 s busy timeout.
    const batch = this.db.query(
      `DELETE FROM messages WHERE id IN
         (SELECT id FROM messages WHERE ts < ? LIMIT ${PURGE_BATCH})`,
    );
    for (;;) {
      const n = this.db.transaction(() => batch.run(cutoffIso).changes).immediate();
      if (n < PURGE_BATCH) break;
    }
    this.db
      .transaction(() => {
        // Parsed status text of a reply older than the window goes with the messages.
        this.db.query("DELETE FROM status WHERE reply_ts < ?").run(cutoffIso);
        this.db.query("DELETE FROM latest_reply WHERE reply_ts < ?").run(cutoffIso);
        this.db
          .query(
            `DELETE FROM tails WHERE path IN
               (SELECT transcript_path FROM sessions WHERE ended_at IS NOT NULL AND ended_at < ?)
             AND path NOT IN
               (SELECT transcript_path FROM sessions WHERE ended_at IS NULL OR ended_at >= ?)`,
          )
          .run(cutoff, cutoff);
        const cnt = this.db
          .query("SELECT COUNT(*) AS n FROM sessions WHERE ended_at IS NOT NULL AND ended_at < ?")
          .get(cutoff) as { n: number };
        deleted = cnt.n;
        this.db
          .query("DELETE FROM sessions WHERE ended_at IS NOT NULL AND ended_at < ?")
          .run(cutoff);
        this.db.query("DELETE FROM sends WHERE created_at < ?").run(cutoff);
      })
      .immediate();
    // The deletes zero the freed pages (secure_delete), but older WAL frames still hold the text
    // until a checkpoint: move them into the database and truncate the WAL now, not at the daily
    // vacuum (PII-03). Best effort: with a reader open it checkpoints what it can and does not throw.
    this.db.run("PRAGMA wal_checkpoint(TRUNCATE)");
    return deleted;
  }

  // ---- sends ----

  createSend(row: SendRow): number {
    const r = this.db
      .query(
        `INSERT INTO sends (session_id, profile, name, sha256, created_at, state, detail)
         VALUES (?, ?, ?, ?, ?, ?, ?)`,
      )
      .run(row.sessionId, row.profile, row.name, row.sha256, row.createdAt, row.state, row.detail);
    return Number(r.lastInsertRowid);
  }

  /**
   * Conditional state change: from pending/held to any state, from failed only to delivered
   * (the relay reported failure but the message arrived). delivered and expired never change.
   * `detail` undefined keeps the stored detail, except that failed -> delivered clears it: the
   * relay's failure reason no longer applies. Returns whether a row changed.
   */
  transitionSend(id: number, to: SendState, detail?: string | null): boolean {
    const where =
      "id = $id AND (state IN ('pending', 'held') OR (state = 'failed' AND $to = 'delivered'))";
    const r =
      detail === undefined
        ? this.db
            .query(
              `UPDATE sends SET state = $to,
                 detail = CASE WHEN state = 'failed' THEN NULL ELSE detail END
               WHERE ${where}`,
            )
            .run({ id, to })
        : this.db
            .query(`UPDATE sends SET state = $to, detail = $detail WHERE ${where}`)
            .run({ id, to, detail });
    return r.changes > 0;
  }

  /** Sends created at or after `since` (epoch ms), in every state and session. */
  countSendsSince(since: number): number {
    const r = this.db.query("SELECT COUNT(*) AS n FROM sends WHERE created_at >= ?").get(since) as {
      n: number;
    };
    return r.n;
  }

  getSend(id: number): SendRow | null {
    const r = this.db.query("SELECT * FROM sends WHERE id = ?").get(id) as Row | null;
    return r ? toSend(r) : null;
  }

  openSendFor(sessionId: string): SendRow | null {
    const r = this.db
      .query(
        `SELECT * FROM sends WHERE session_id = ? AND state IN ('pending', 'held')
          ORDER BY id DESC LIMIT 1`,
      )
      .get(sessionId) as Row | null;
    return r ? toSend(r) : null;
  }

  /** Every pending/held send, oldest first; only one session's when sessionId is given. */
  openSends(sessionId?: string): SendRow[] {
    const rows = (
      sessionId === undefined
        ? this.db.query("SELECT * FROM sends WHERE state IN ('pending', 'held') ORDER BY id").all()
        : this.db
            .query(
              `SELECT * FROM sends WHERE session_id = ? AND state IN ('pending', 'held')
                ORDER BY id`,
            )
            .all(sessionId)
    ) as Row[];
    return rows.map(toSend);
  }

  /**
   * One session's sends that transcript evidence can still change, oldest first: pending/held,
   * plus failed ones created at or after `failedSince` (they may still prove delivered).
   */
  /** SHA-256 of every send made to this session since `since` (epoch ms), in any state. */
  sendHashesSince(sessionId: string, since: number): Set<string> {
    const rows = this.db
      .query("SELECT sha256 FROM sends WHERE session_id = ? AND created_at >= ?")
      .all(sessionId, since) as { sha256: string }[];
    return new Set(rows.map((r) => r.sha256));
  }

  sendsToVerify(sessionId: string, failedSince: number): SendRow[] {
    const rows = this.db
      .query(
        `SELECT * FROM sends WHERE session_id = ?
           AND (state IN ('pending', 'held') OR (state = 'failed' AND created_at >= ?))
         ORDER BY id`,
      )
      .all(sessionId, failedSince) as Row[];
    return rows.map(toSend);
  }

  // ---- meta ----

  setMeta(key: string, value: string | null): void {
    if (value === null) {
      this.db.query("DELETE FROM meta WHERE key = ?").run(key);
      return;
    }
    this.db
      .query(
        "INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
      )
      .run(key, value);
  }

  /** Changes whenever another connection commits to the file; own writes leave it unchanged. */
  dataVersion(): number {
    const r = this.db.query("PRAGMA data_version").get() as { data_version: number };
    return r.data_version;
  }

  getMeta(key: string): string | null {
    const r = this.db.query("SELECT value FROM meta WHERE key = ?").get(key) as {
      value: string;
    } | null;
    return r?.value ?? null;
  }
}
