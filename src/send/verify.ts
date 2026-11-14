import { createHash } from "node:crypto";
import {
  DELIVERY_WINDOW_MS,
  EVAL_SESSION_PREFIX,
  MESSAGE_HEADER,
  RELAY_NAME,
  RETENTION_MS,
} from "../config.ts";
import type { Classified } from "../ingest/records.ts";
import type { SendRow, SendState, Store } from "../store/db.ts";

/** Records up to this much before createdAt still count (clock skew between hub and recipient). */
const SKEW_MS = 1000;

const TERMINAL: ReadonlySet<SendState> = new Set(["delivered", "expired"]);

/** The exact text the relay puts on SendMessage: header, blank line, the owner's text unchanged. */
export function buildMessage(text: string): string {
  return `${MESSAGE_HEADER}\n\n${text}`;
}

/** CRLF -> LF, then trailing whitespace trimmed (end of string only). */
export function normalize(text: string): string {
  return text.replaceAll("\r\n", "\n").trimEnd();
}

export function sha256Hex(text: string): string {
  return createHash("sha256").update(text, "utf8").digest("hex");
}

function after(ts: string, createdAt: number): boolean {
  const t = Date.parse(ts);
  return Number.isFinite(t) && t >= createdAt - SKEW_MS;
}

/** Detail of a send proved by a body the relay sent without the header. */
export const HEADERLESS_DETAIL = "delivered without the Session Hub header";

/**
 * How a record proves this send: "full" when its body is the whole message, "headerless" when the
 * relay model dropped the header line (it sometimes does) and the body is exactly the owner's text.
 * The send row keeps only the hash of the whole message, so the header is put back before hashing.
 */
function match(send: SendRow, r: Classified): "full" | "headerless" | null {
  if (r.kind !== "peer-message" || r.fromName !== RELAY_NAME || !after(r.ts, send.createdAt)) {
    return null;
  }
  const h = bodyHashes(r.body);
  if (h.full === send.sha256) return "full";
  if (h.headerless === send.sha256) return "headerless";
  return null;
}

/**
 * The send hashes a relay body can prove: the body as the whole message, and, when it lacks the
 * header, the body with the header put back (match and the stray check use the same two).
 */
function bodyHashes(raw: string): { full: string; headerless: string | null } {
  const body = normalize(raw);
  return {
    full: sha256Hex(body),
    headerless: body.startsWith(MESSAGE_HEADER) ? null : sha256Hex(normalize(buildMessage(body))),
  };
}

/** Index of the record that proves this send arrived, or -1. Records must be the target's own. */
function deliveryIndex(send: SendRow, records: readonly Classified[]): number {
  return records.findIndex((r) => match(send, r) !== null);
}

/** How the first proving record matched (see match), or null when none proves the send. */
export function deliveryMatch(
  send: SendRow,
  records: readonly Classified[],
): "full" | "headerless" | null {
  const i = deliveryIndex(send, records);
  return i < 0 ? null : match(send, records[i] as Classified);
}

/**
 * New state of a send given records from the target sessionId's transcript only.
 * delivered > held > expired > unchanged. delivered and expired never change. failed changes
 * only to delivered, on an exact match while inside DELIVERY_WINDOW_MS (the relay reported
 * failure or timed out, but the message did arrive).
 */
export function checkDelivery(send: SendRow, records: Classified[], now: number): SendState {
  if (TERMINAL.has(send.state)) return send.state;
  const inWindow = now - send.createdAt <= DELIVERY_WINDOW_MS;
  if (send.state === "failed") {
    return inWindow && deliveryIndex(send, records) >= 0 ? "delivered" : "failed";
  }
  if (deliveryIndex(send, records) >= 0) return "delivered";
  if (
    send.state === "pending" &&
    records.some(
      (r) => r.kind === "held-notice" && r.peerName === RELAY_NAME && after(r.ts, send.createdAt),
    )
  ) {
    return "held";
  }
  if (!inWindow) return "expired";
  return send.state;
}

/** A relay message that matches no send to the session it landed in. */
export interface StrayRelay {
  sessionId: string;
  ts: string;
}

/**
 * Flags every relay message in these records whose text matches no send made to this session
 * within retention, in any state (LLM-01): the relay model wrote to a session or a text the owner
 * did not send. Counted in meta relay_stray_count, with the time of the last one in relay_stray_last;
 * the text is never stored or logged. The relay eval's own fixture sessions (EVAL_SESSION_PREFIX)
 * receive relays the hub did not send, by design, and are skipped.
 */
function reportStrays(
  store: Store,
  sessionId: string,
  records: readonly Classified[],
  now: number,
  onStray?: (s: StrayRelay) => void,
): void {
  const relay = records.filter((r) => r.kind === "peer-message" && r.fromName === RELAY_NAME);
  if (relay.length === 0) return;
  if (store.getSession(sessionId)?.name.startsWith(EVAL_SESSION_PREFIX)) return;
  const known = store.sendHashesSince(sessionId, now - RETENTION_MS);
  for (const r of relay) {
    if (r.kind !== "peer-message") continue;
    const h = bodyHashes(r.body);
    if (known.has(h.full) || (h.headerless !== null && known.has(h.headerless))) continue;
    // Older than retention: its send row may be purged (a transcript read again from the start).
    const t = Date.parse(r.ts);
    if (!Number.isFinite(t) || t < now - RETENTION_MS) continue;
    const n = Number(store.getMeta("relay_stray_count") ?? "0");
    store.setMeta("relay_stray_count", String((Number.isFinite(n) ? n : 0) + 1));
    store.setMeta("relay_stray_last", String(now));
    onStray?.({ sessionId, ts: r.ts });
  }
}

/**
 * Returns the per-pass hook the daemon calls with a session's new records. Sends are checked
 * oldest first (pending/held, plus failed ones still inside the window), and a delivery record
 * proves at most one send, so two overridden identical messages need two records. A
 * held-notice names no message, so it applies only to the session's newest pending send. Every
 * change is a conditional store transition, so a concurrent writer can never undo delivered.
 */
export function deliveryChecker(
  store: Store,
  now: () => number,
  onStray?: (s: StrayRelay) => void,
): (sessionId: string, records: Classified[]) => void {
  return (sessionId, records) => {
    const t = now();
    reportStrays(store, sessionId, records, t, onStray);
    const sends = store.sendsToVerify(sessionId, t - DELIVERY_WINDOW_MS);
    if (sends.length === 0) return;
    let newestPending: number | undefined;
    for (const s of sends) if (s.state === "pending") newestPending = s.id;
    const available = [...records];
    for (const s of sends) {
      if (s.id === undefined) continue;
      const recs =
        s.id === newestPending ? available : available.filter((r) => r.kind !== "held-notice");
      const idx = deliveryIndex(s, available);
      const next = checkDelivery(s, recs, t);
      const how = next === "delivered" && idx >= 0 ? match(s, available[idx] as Classified) : null;
      if (next === "delivered" && idx >= 0) available.splice(idx, 1);
      // The store clears a failed send's detail when it moves to delivered; a body without the
      // header is noted instead.
      if (next !== s.state) {
        store.transitionSend(s.id, next, how === "headerless" ? HEADERLESS_DETAIL : undefined);
      }
    }
  };
}

/** Marks pending/held sends older than the delivery window expired. Returns how many. */
export function expireSends(store: Store, now: () => number): number {
  const t = now();
  let n = 0;
  for (const s of store.openSends()) {
    if (s.id !== undefined && t - s.createdAt > DELIVERY_WINDOW_MS) {
      if (store.transitionSend(s.id, "expired")) n += 1;
    }
  }
  return n;
}
