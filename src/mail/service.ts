import { maskSecrets } from "../security/mask.ts";
import type { Store } from "../store/db.ts";
import {
  HUB_ROLE,
  isRoleName,
  knownRoles,
  OWNER_ROLE,
  type RolesConfig,
  roleOfSession,
} from "./roles.ts";
import {
  type AnswerResult,
  type DecisionRow,
  MAIL_KINDS,
  type MailKind,
  type MailRow,
} from "./store.ts";

/** Who is calling: a session's role, or the owner (no session). */
export interface Caller {
  role: string;
  sessionId: string | null;
}

export type MailRefuseReason =
  | "no-session"
  | "session-unknown"
  | "session-ended"
  | "no-role"
  | "bad-role"
  | "unknown-role"
  | "own-role"
  | "bad-thread"
  | "bad-kind"
  | "empty-body"
  | "too-long"
  | "evidence-required"
  | "looks-secret"
  | "bad-ttl"
  | "rate-limited"
  | "bad-options"
  | "not-owner";

export class MailRefused extends Error {
  constructor(
    readonly reason: MailRefuseReason,
    detail?: string,
  ) {
    super(detail === undefined ? `refused: ${reason}` : `refused: ${reason}: ${detail}`);
    this.name = "MailRefused";
  }
}

export const MAX_BODY_CHARS = 20_000;
export const MAX_EVIDENCE_CHARS = 2_000;
export const MAX_QUESTION_CHARS = 4_000;
export const MAX_ANSWER_CHARS = 4_000;
const MAX_OPTIONS = 10;
const MAX_OPTION_CHARS = 200;
const MAX_RECOMMENDATION_CHARS = 1_000;
const MAX_TTL_HOURS = 7 * 24;
/** Mail one session may send in any rolling minute (a looping session cannot flood the hub). */
export const MAX_MAIL_PER_MINUTE = 30;
const RATE_WINDOW_MS = 60_000;
const CALLER_POLL_MS = 500;

/** A thread names a decision or work item: "D-0031", "PUSH-TEST", "issue#12". */
const THREAD_RE = /^[A-Za-z0-9][A-Za-z0-9._:#/-]{0,63}$/;

/** Kinds whose sender must say where the facts come from. */
const NEEDS_EVIDENCE: ReadonlySet<MailKind> = new Set(["answer", "chat-out"]);

export function mailId(id: number): string {
  return `M-${String(id).padStart(6, "0")}`;
}

/** "M-000417", "M-417", "m-417" or "417"; null when it is none of those. */
export function parseMailId(s: string): number | null {
  const m = /^(?:[Mm]-)?(\d{1,15})$/.exec(s.trim());
  if (!m) return null;
  const n = Number(m[1]);
  return Number.isSafeInteger(n) && n > 0 ? n : null;
}

function looksSecret(text: string): boolean {
  return maskSecrets(text) !== text;
}

/** True when this process runs inside a Claude Code session (its Bash tool, a hook, a Monitor). */
export function insideClaudeCode(env: Record<string, string | undefined>): boolean {
  return Boolean(env.CLAUDE_CODE_SESSION_ID || env.CLAUDECODE);
}

/**
 * The caller: outside Claude Code it is the owner; inside, the session named by
 * CLAUDE_CODE_SESSION_ID, which the daemon must have indexed (waited for up to `waitMs`) and which
 * must be live and have a role.
 */
export async function resolveCaller(
  store: Store,
  cfg: RolesConfig,
  env: Record<string, string | undefined>,
  opts: { waitMs: number; sleep?: (ms: number) => Promise<void> },
): Promise<Caller> {
  const sid = env.CLAUDE_CODE_SESSION_ID;
  if (!sid) {
    if (env.CLAUDECODE) {
      throw new MailRefused(
        "no-session",
        "inside Claude Code but CLAUDE_CODE_SESSION_ID is not set",
      );
    }
    return { role: OWNER_ROLE, sessionId: null };
  }
  const sleep = opts.sleep ?? ((ms: number) => Bun.sleep(ms));
  let waited = 0;
  let s = store.getSession(sid);
  while (s === null) {
    if (waited >= opts.waitMs) {
      throw new MailRefused(
        "session-unknown",
        "this session is not in the hub index yet; is the hub daemon running?",
      );
    }
    await sleep(CALLER_POLL_MS);
    waited += CALLER_POLL_MS;
    s = store.getSession(sid);
  }
  if (s.endedAt !== null) throw new MailRefused("session-ended");
  const r = roleOfSession(cfg, s);
  if ("error" in r) throw new MailRefused("no-role", r.error);
  return { role: r.role, sessionId: sid };
}

/** Roles of the live (not ended) indexed sessions, with the session names behind each. */
export function liveSessionsByRole(store: Store, cfg: RolesConfig): Map<string, string[]> {
  const out = new Map<string, string[]>();
  for (const s of store.sessionIdentities()) {
    if (s.endedAt !== null) continue;
    const r = roleOfSession(cfg, s);
    if (!("role" in r)) continue;
    const names = out.get(r.role) ?? [];
    names.push(s.name);
    out.set(r.role, names);
  }
  return out;
}

export interface SendMailRequest {
  to: string;
  thread: string;
  kind: MailKind;
  body: string;
  evidence: string | null;
  ttlHours?: number;
}

/** Checks the request and queues the mail; refusals throw MailRefused and store nothing. */
export function sendMail(
  store: Store,
  cfg: RolesConfig,
  caller: Caller,
  req: SendMailRequest,
  now: number,
): MailRow {
  if (!isRoleName(req.to)) throw new MailRefused("bad-role", req.to);
  if (req.to === caller.role) throw new MailRefused("own-role");
  if (req.to === HUB_ROLE || !knownRoles(cfg, store.sessionIdentities()).has(req.to)) {
    throw new MailRefused(
      "unknown-role",
      `no role "${req.to}" in roles.json or among indexed sessions (hub roles lists them)`,
    );
  }
  if (!THREAD_RE.test(req.thread)) throw new MailRefused("bad-thread", req.thread);
  if (!(MAIL_KINDS as readonly string[]).includes(req.kind)) {
    throw new MailRefused("bad-kind", `use one of ${MAIL_KINDS.join(", ")}`);
  }
  if (req.body.trim() === "") throw new MailRefused("empty-body");
  if (req.body.length > MAX_BODY_CHARS) {
    throw new MailRefused("too-long", `body over ${MAX_BODY_CHARS} characters`);
  }
  const evidence = req.evidence?.trim() ? req.evidence : null;
  if (NEEDS_EVIDENCE.has(req.kind) && evidence === null) {
    throw new MailRefused("evidence-required", `kind ${req.kind} needs --evidence`);
  }
  if (evidence !== null && evidence.length > MAX_EVIDENCE_CHARS) {
    throw new MailRefused("too-long", `evidence over ${MAX_EVIDENCE_CHARS} characters`);
  }
  if (looksSecret(req.body) || (evidence !== null && looksSecret(evidence))) {
    throw new MailRefused("looks-secret", "never put tokens, keys or passwords in mail");
  }
  let ttlMs = cfg.expiryMs;
  if (req.ttlHours !== undefined) {
    if (!Number.isFinite(req.ttlHours) || req.ttlHours <= 0 || req.ttlHours > MAX_TTL_HOURS) {
      throw new MailRefused("bad-ttl", `1 to ${MAX_TTL_HOURS} hours`);
    }
    ttlMs = req.ttlHours * 3_600_000;
  }
  const mail = store.mail();
  return store.tx(() => {
    if (
      caller.sessionId !== null &&
      mail.countSentSince(caller.sessionId, now - RATE_WINDOW_MS) >= MAX_MAIL_PER_MINUTE
    ) {
      throw new MailRefused("rate-limited", `at most ${MAX_MAIL_PER_MINUTE} per minute`);
    }
    return mail.insert({
      thread: req.thread,
      fromRole: caller.role,
      fromSession: caller.sessionId,
      toRole: req.to,
      kind: req.kind,
      body: req.body,
      evidence,
      createdAt: now,
      expiresAt: now + ttlMs,
    });
  });
}

export interface AskRequest {
  question: string;
  options: string[];
  recommendation: string | null;
}

/** Opens a decision for the caller's role. */
export function askDecision(
  store: Store,
  caller: Caller,
  req: AskRequest,
  now: number,
): DecisionRow {
  if (caller.sessionId === null) throw new MailRefused("no-session", "hub ask runs in a session");
  if (req.question.trim() === "") throw new MailRefused("empty-body", "no question");
  if (req.question.length > MAX_QUESTION_CHARS) {
    throw new MailRefused("too-long", `question over ${MAX_QUESTION_CHARS} characters`);
  }
  if (
    req.options.length > MAX_OPTIONS ||
    req.options.some((o) => o.trim() === "" || o.length > MAX_OPTION_CHARS)
  ) {
    throw new MailRefused(
      "bad-options",
      `at most ${MAX_OPTIONS} options of 1 to ${MAX_OPTION_CHARS} characters`,
    );
  }
  const rec = req.recommendation?.trim() ? req.recommendation : null;
  if (rec !== null && rec.length > MAX_RECOMMENDATION_CHARS) {
    throw new MailRefused("too-long", `recommendation over ${MAX_RECOMMENDATION_CHARS} characters`);
  }
  if ([req.question, ...req.options, rec ?? ""].some(looksSecret)) {
    throw new MailRefused("looks-secret", "never put tokens, keys or passwords in a decision");
  }
  return store.mail().createDecision({
    role: caller.role,
    question: req.question,
    options: req.options,
    recommendation: rec,
    fromSession: caller.sessionId,
    createdAt: now,
  });
}

/**
 * Stores the owner's answer once and mails it to the decision's role in the same transaction.
 * "same" and "conflict" send nothing; "replaced" (only with `replace`) mails the change.
 */
export function answerAndNotify(
  store: Store,
  cfg: RolesConfig,
  id: string,
  answer: string,
  via: string,
  now: number,
  opts: { replace?: boolean } = {},
): { result: AnswerResult; row: DecisionRow | null } {
  if (answer.trim() === "") throw new MailRefused("empty-body", "no answer");
  if (answer.length > MAX_ANSWER_CHARS) {
    throw new MailRefused("too-long", `answer over ${MAX_ANSWER_CHARS} characters`);
  }
  if (looksSecret(answer)) throw new MailRefused("looks-secret", "never answer with a secret");
  const mail = store.mail();
  return store.tx(() => {
    const r = mail.answerDecision(id, answer, via, now, opts);
    if ((r.result === "answered" || r.result === "replaced") && r.row !== null) {
      const was = r.row.conflicts.at(-1)?.answer;
      const body =
        r.result === "answered"
          ? `${id} answered by the owner: ${answer}`
          : `${id} changed by the owner: ${answer} (was: ${was ?? ""})`;
      mail.insert({
        thread: id,
        fromRole: OWNER_ROLE,
        fromSession: null,
        toRole: r.row.role,
        kind: "decision",
        body,
        evidence: null,
        createdAt: now,
        expiresAt: now + cfg.expiryMs,
      });
    }
    return r;
  });
}

export interface UpkeepResult {
  expired: number;
  requeued: number;
  flagged: number;
}

/**
 * The daemon's pass: delivered mail of ended sessions goes back to the queue; queued mail past its
 * expiry expires; delivered mail unacknowledged after the timeout is flagged. Expired and flagged
 * mail each tell their sender once, by mail from the hub (never about the hub's own mail).
 */
export function mailUpkeep(store: Store, cfg: RolesConfig, now: number): UpkeepResult {
  const mail = store.mail();
  return store.tx(() => {
    const requeued = mail.requeueEnded();
    const expired = mail.expireQueued(now);
    const flagged = mail.flagUnacked(now, cfg.ackTimeoutMs);
    const minutes = Math.round(cfg.ackTimeoutMs / 60_000);
    const notify = (m: MailRow, body: string) => {
      if (m.fromRole === HUB_ROLE) return;
      mail.insert({
        thread: m.thread,
        fromRole: HUB_ROLE,
        fromSession: null,
        toRole: m.fromRole,
        kind: "fyi",
        body,
        evidence: null,
        createdAt: now,
        expiresAt: now + cfg.expiryMs,
      });
    };
    for (const m of expired) {
      notify(
        m,
        `${mailId(m.id)} to ${m.toRole} expired: no session of ${m.toRole} took it before ${new Date(m.expiresAt).toISOString()}.`,
      );
    }
    for (const m of flagged) {
      notify(m, `${mailId(m.id)} to ${m.toRole} has no ack ${minutes} min after it was delivered.`);
    }
    return { expired: expired.length, requeued: requeued.length, flagged: flagged.length };
  });
}
