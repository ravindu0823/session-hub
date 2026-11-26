import type { Database } from "bun:sqlite";
import { createHash } from "node:crypto";
import { HUB_ROLE } from "./roles.ts";

export const MAIL_KINDS = ["task", "answer", "fyi", "decision", "chat-in", "chat-out"] as const;
export type MailKind = (typeof MAIL_KINDS)[number];
export type MailState = "queued" | "delivered" | "acked" | "done" | "expired";

export interface NewMail {
  thread: string;
  fromRole: string;
  fromSession: string | null;
  toRole: string;
  kind: MailKind;
  body: string;
  evidence: string | null;
  createdAt: number;
  expiresAt: number;
}

export interface MailRow extends NewMail {
  id: number;
  bodySha: string;
  state: MailState;
  deliveredAt: number | null;
  deliveredSession: string | null;
  /** How many times it was handed to a session (it goes back to the queue when that session ends). */
  deliveries: number;
  ackedAt: number | null;
  doneAt: number | null;
  /** When the sender was told it is still unacknowledged after the timeout. */
  flaggedAt: number | null;
}

export type DecisionState = "open" | "answered" | "closed";

export interface DecisionConflict {
  answer: string;
  at: number;
  via: string;
}

export interface NewDecision {
  role: string;
  question: string;
  options: string[];
  recommendation: string | null;
  fromSession: string | null;
  createdAt: number;
}

export interface DecisionRow extends NewDecision {
  id: string;
  seq: number;
  state: DecisionState;
  answer: string | null;
  answeredAt: number | null;
  answeredVia: string | null;
  /** Answers that were not applied (a different second answer) or were replaced. */
  conflicts: DecisionConflict[];
  closedAt: number | null;
}

export type MailChange =
  | "ok"
  | "not-found"
  | "not-yours"
  | "not-delivered"
  | "other-session"
  | "already";
export type AnswerResult = "answered" | "same" | "conflict" | "replaced" | "not-found" | "closed";

/** Mail claimed per read: a session's backlog arrives in batches of this size. */
const CLAIM_BATCH = 50;

type Row = Record<string, unknown>;

const num = (v: unknown): number | null => (v === null || v === undefined ? null : (v as number));
const str = (v: unknown): string | null => (v === null || v === undefined ? null : (v as string));

function toMail(r: Row): MailRow {
  return {
    id: r.id as number,
    thread: r.thread as string,
    fromRole: r.from_role as string,
    fromSession: str(r.from_session),
    toRole: r.to_role as string,
    kind: r.kind as MailKind,
    body: r.body as string,
    bodySha: r.body_sha as string,
    evidence: str(r.evidence),
    state: r.state as MailState,
    createdAt: r.created_at as number,
    deliveredAt: num(r.delivered_at),
    deliveredSession: str(r.delivered_session),
    deliveries: r.deliveries as number,
    ackedAt: num(r.acked_at),
    doneAt: num(r.done_at),
    expiresAt: r.expires_at as number,
    flaggedAt: num(r.flagged_at),
  };
}

function toDecision(r: Row): DecisionRow {
  return {
    id: r.id as string,
    seq: r.seq as number,
    role: r.role as string,
    question: r.question as string,
    options: JSON.parse(r.options_json as string) as string[],
    recommendation: str(r.recommendation),
    state: r.state as DecisionState,
    answer: str(r.answer),
    answeredAt: num(r.answered_at),
    answeredVia: str(r.answered_via),
    conflicts: JSON.parse(r.conflicts_json as string) as DecisionConflict[],
    createdAt: r.created_at as number,
    closedAt: num(r.closed_at),
    fromSession: str(r.from_session),
  };
}

export function sha256Text(text: string): string {
  return createHash("sha256").update(text, "utf8").digest("hex");
}

/** "D-0001": at least four digits. */
export function decisionId(seq: number): string {
  return `D-${String(seq).padStart(4, "0")}`;
}

/**
 * Mail and decisions in hub.db. Every change is one conditional statement, so two processes (two
 * sessions, the daemon, the tray's bridge) never both apply the same step.
 */
export class MailStore {
  constructor(private readonly db: Database) {}

  insert(m: NewMail): MailRow {
    const r = this.db
      .query(
        `INSERT INTO mail (thread, from_role, from_session, to_role, kind, body, body_sha, evidence,
           state, created_at, expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'queued', ?, ?) RETURNING *`,
      )
      .get(
        m.thread,
        m.fromRole,
        m.fromSession,
        m.toRole,
        m.kind,
        m.body,
        sha256Text(m.body),
        m.evidence,
        m.createdAt,
        m.expiresAt,
      ) as Row;
    return toMail(r);
  }

  get(id: number): MailRow | null {
    const r = this.db.query("SELECT * FROM mail WHERE id = ?").get(id) as Row | null;
    return r ? toMail(r) : null;
  }

  /**
   * Hands queued mail for a role to one session (null: the owner, outside Claude Code), oldest
   * first; nobody else gets those rows.
   */
  claim(toRole: string, sessionId: string | null, now: number): MailRow[] {
    const rows = this.db
      .query(
        `UPDATE mail SET state = 'delivered', delivered_at = $now, delivered_session = $sid,
           deliveries = deliveries + 1
         WHERE id IN (SELECT id FROM mail WHERE to_role = $role AND state = 'queued'
                       ORDER BY id LIMIT ${CLAIM_BATCH})
         RETURNING *`,
      )
      .all({ now, sid: sessionId, role: toRole }) as Row[];
    return rows.map(toMail).sort((a, b) => a.id - b.id);
  }

  /** A cheap read: does this role have queued mail? (followers poll it before they claim). */
  hasQueued(toRole: string): boolean {
    return (
      this.db
        .query("SELECT 1 FROM mail WHERE to_role = ? AND state = 'queued' LIMIT 1")
        .get(toRole) !== null
    );
  }

  /** Mail delivered to this session and not acknowledged yet, oldest first. */
  unackedFor(sessionId: string): MailRow[] {
    const rows = this.db
      .query("SELECT * FROM mail WHERE delivered_session = ? AND state = 'delivered' ORDER BY id")
      .all(sessionId) as Row[];
    return rows.map(toMail);
  }

  /**
   * The recipient read it. Only the session the mail was delivered to may ack it (null: the owner,
   * who takes mail without a session), so another session of the same role cannot hide it from the
   * session that has it.
   */
  ack(id: number, role: string, sessionId: string | null, now: number): MailChange {
    const m = this.get(id);
    if (!m) return "not-found";
    if (m.toRole !== role) return "not-yours";
    if (m.state === "queued") return "not-delivered";
    if (m.state !== "delivered") return "already";
    if (m.deliveredSession !== sessionId) return "other-session";
    const r = this.db
      .query(
        `UPDATE mail SET state = 'acked', acked_at = ? WHERE id = ? AND state = 'delivered'
           AND delivered_session IS ?`,
      )
      .run(now, id, sessionId);
    return r.changes > 0 ? "ok" : "already";
  }

  /** The sender or the recipient closes the thread step; queued and expired mail can be closed too. */
  done(id: number, role: string, now: number): MailChange {
    const m = this.get(id);
    if (!m) return "not-found";
    if (m.toRole !== role && m.fromRole !== role) return "not-yours";
    const r = this.db
      .query("UPDATE mail SET state = 'done', done_at = ? WHERE id = ? AND state <> 'done'")
      .run(now, id);
    return r.changes > 0 ? "ok" : "already";
  }

  /** Mail to a role that is still open (queued or delivered), or in every state with `all`. */
  inbox(toRole: string, opts: { all?: boolean } = {}): MailRow[] {
    const rows = (
      opts.all
        ? this.db.query("SELECT * FROM mail WHERE to_role = ? ORDER BY id")
        : this.db.query(
            "SELECT * FROM mail WHERE to_role = ? AND state IN ('queued', 'delivered') ORDER BY id",
          )
    ).all(toRole) as Row[];
    return rows.map(toMail);
  }

  /** A role's sent mail, newest first. */
  sent(fromRole: string, limit: number): MailRow[] {
    const rows = this.db
      .query("SELECT * FROM mail WHERE from_role = ? ORDER BY id DESC LIMIT ?")
      .all(fromRole, limit) as Row[];
    return rows.map(toMail);
  }

  countSentSince(fromSession: string, since: number): number {
    const r = this.db
      .query("SELECT COUNT(*) AS n FROM mail WHERE from_session = ? AND created_at >= ?")
      .get(fromSession, since) as { n: number };
    return r.n;
  }

  /** Mail a role sent without a session (the owner, outside Claude Code) since `since`. */
  countSentByRoleSince(fromRole: string, since: number): number {
    const r = this.db
      .query(
        `SELECT COUNT(*) AS n FROM mail WHERE from_role = ? AND from_session IS NULL
           AND created_at >= ?`,
      )
      .get(fromRole, since) as { n: number };
    return r.n;
  }

  queuedByRole(): Map<string, number> {
    const rows = this.db
      .query(
        "SELECT to_role, COUNT(*) AS n FROM mail WHERE state = 'queued' GROUP BY to_role ORDER BY to_role",
      )
      .all() as { to_role: string; n: number }[];
    return new Map(rows.map((r) => [r.to_role, r.n]));
  }

  /** Delivered mail still unacknowledged after the sender was told about it. */
  countFlagged(): number {
    const r = this.db
      .query("SELECT COUNT(*) AS n FROM mail WHERE state = 'delivered' AND flagged_at IS NOT NULL")
      .get() as { n: number };
    return r.n;
  }

  /** Open (queued or delivered) mail to a role. */
  countOpenTo(toRole: string): number {
    const r = this.db
      .query(
        "SELECT COUNT(*) AS n FROM mail WHERE to_role = ? AND state IN ('queued', 'delivered')",
      )
      .get(toRole) as { n: number };
    return r.n;
  }

  // ---- upkeep (the daemon) ----

  /** Queued mail past its expiry; returns the rows that expired now. */
  expireQueued(now: number): MailRow[] {
    const rows = this.db
      .query(
        "UPDATE mail SET state = 'expired' WHERE state = 'queued' AND expires_at <= ? RETURNING *",
      )
      .all(now) as Row[];
    return rows.map(toMail).sort((a, b) => a.id - b.id);
  }

  /**
   * Delivered, unacknowledged mail whose session has ended, or is unknown to the index, goes back to
   * the queue: the next session of that role gets it. Mail the owner read (no session) stays.
   */
  requeueEnded(): MailRow[] {
    const rows = this.db
      .query(
        `UPDATE mail SET state = 'queued', delivered_at = NULL, delivered_session = NULL
         WHERE state = 'delivered' AND delivered_session IS NOT NULL AND (NOT EXISTS (
           SELECT 1 FROM sessions s WHERE s.session_id = mail.delivered_session AND s.ended_at IS NULL))
         RETURNING *`,
      )
      .all() as Row[];
    return rows.map(toMail).sort((a, b) => a.id - b.id);
  }

  /** Delivered mail unacknowledged for `timeoutMs`, flagged once. Mail from the hub itself is not. */
  flagUnacked(now: number, timeoutMs: number): MailRow[] {
    const rows = this.db
      .query(
        `UPDATE mail SET flagged_at = $now
         WHERE state = 'delivered' AND flagged_at IS NULL AND delivered_at <= $cutoff
           AND from_role <> $hub
         RETURNING *`,
      )
      .all({ now, cutoff: now - timeoutMs, hub: HUB_ROLE }) as Row[];
    return rows.map(toMail).sort((a, b) => a.id - b.id);
  }

  // ---- decisions ----

  createDecision(d: NewDecision): DecisionRow {
    return this.db
      .transaction(() => {
        // The AUTOINCREMENT high-water mark, not MAX(seq): a purged decision's id is never handed out again.
        const r = this.db
          .query(
            "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'decisions'), 0) + 1 AS n",
          )
          .get() as { n: number };
        const row = this.db
          .query(
            `INSERT INTO decisions (id, seq, role, question, options_json, recommendation, state,
               created_at, from_session)
             VALUES (?, ?, ?, ?, ?, ?, 'open', ?, ?) RETURNING *`,
          )
          .get(
            decisionId(r.n),
            r.n,
            d.role,
            d.question,
            JSON.stringify(d.options),
            d.recommendation,
            d.createdAt,
            d.fromSession,
          ) as Row;
        return toDecision(row);
      })
      .immediate();
  }

  getDecision(id: string): DecisionRow | null {
    const r = this.db.query("SELECT * FROM decisions WHERE id = ?").get(id) as Row | null;
    return r ? toDecision(r) : null;
  }

  listDecisions(f: { role?: string; state?: DecisionState }): DecisionRow[] {
    const rows = this.db
      .query(
        `SELECT * FROM decisions WHERE ($role IS NULL OR role = $role)
           AND ($state IS NULL OR state = $state) ORDER BY seq`,
      )
      .all({ role: f.role ?? null, state: f.state ?? null }) as Row[];
    return rows.map(toDecision);
  }

  /**
   * Stores the owner's answer once. The same text again is "same" (nothing changes); a different
   * text is a "conflict", recorded but never applied, unless `replace` says the owner means it.
   */
  answerDecision(
    id: string,
    answer: string,
    via: string,
    now: number,
    opts: { replace?: boolean } = {},
  ): { result: AnswerResult; row: DecisionRow | null } {
    return this.db
      .transaction(() => {
        const d = this.getDecision(id);
        if (!d) return { result: "not-found" as const, row: null };
        if (d.state === "closed") return { result: "closed" as const, row: d };
        if (d.state === "open") {
          this.db
            .query(
              `UPDATE decisions SET state = 'answered', answer = ?, answered_at = ?, answered_via = ?
               WHERE id = ?`,
            )
            .run(answer, now, via, id);
          return { result: "answered" as const, row: this.getDecision(id) };
        }
        if (d.answer === answer) return { result: "same" as const, row: d };
        if (opts.replace) {
          const old: DecisionConflict = {
            answer: d.answer ?? "",
            at: d.answeredAt ?? 0,
            via: d.answeredVia ?? "",
          };
          this.db
            .query(
              `UPDATE decisions SET answer = ?, answered_at = ?, answered_via = ?, conflicts_json = ?
               WHERE id = ?`,
            )
            .run(answer, now, via, JSON.stringify([...d.conflicts, old]), id);
          return { result: "replaced" as const, row: this.getDecision(id) };
        }
        this.db
          .query("UPDATE decisions SET conflicts_json = ? WHERE id = ?")
          .run(JSON.stringify([...d.conflicts, { answer, at: now, via }]), id);
        return { result: "conflict" as const, row: this.getDecision(id) };
      })
      .immediate();
  }

  /** The owning role closes its decision. */
  closeDecision(id: string, role: string, now: number): MailChange {
    const d = this.getDecision(id);
    if (!d) return "not-found";
    if (d.role !== role) return "not-yours";
    const r = this.db
      .query(
        "UPDATE decisions SET state = 'closed', closed_at = ? WHERE id = ? AND state <> 'closed'",
      )
      .run(now, id);
    return r.changes > 0 ? "ok" : "already";
  }

  /**
   * Retention (PII-03): open decisions created before the cutoff are closed now, so the purge removes
   * them a retention window later. Returns the rows closed by this call.
   */
  closeStaleDecisions(cutoff: number, now: number): DecisionRow[] {
    const rows = this.db
      .query(
        `UPDATE decisions SET state = 'closed', closed_at = ?
         WHERE state = 'open' AND created_at < ? RETURNING id`,
      )
      .all(now, cutoff) as { id: string }[];
    return rows
      .map((r) => this.getDecision(r.id))
      .filter((d): d is DecisionRow => d !== null)
      .sort((a, b) => a.seq - b.seq);
  }

  /**
   * Retention: mail older than the cutoff, except queued mail (the upkeep expires it first, so its
   * sender is told), and settled decisions closed or answered before the cutoff. Open decisions stay;
   * the upkeep closes them after OPEN_DECISION_MAX_MS (closeStaleDecisions).
   */
  purge(cutoff: number): void {
    this.db.query("DELETE FROM mail WHERE created_at < ? AND state <> 'queued'").run(cutoff);
    this.db
      .query(
        `DELETE FROM decisions WHERE state <> 'open'
           AND COALESCE(closed_at, answered_at, created_at) < ?`,
      )
      .run(cutoff);
  }
}
