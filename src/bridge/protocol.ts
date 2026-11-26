import type { SendState, SessionView } from "../store/db.ts";
import type { HubSnapshot } from "../view/snapshot.ts";

// Wire format between `hub bridge` and the tray app: one UTF-8 JSON object per `\n`-terminated
// line, at most MAX_LINE_BYTES per line, both ways. stdout carries only these lines.

export const PROTOCOL = 1;
export const MAX_LINE_BYTES = 1_048_576;
/** How often the bridge checks the index for changes (data_version, the daemon's pid). */
export const POLL_MS = 500;
export const MIN_EMIT_GAP_MS = 250;
export const HEARTBEAT_MS = 15_000;
/** history and search limits above this are lowered to it. */
export const MAX_LIMIT = 500;

export type Request =
  | {
      id: number;
      type: "history";
      sessionId: string;
      limit: number;
      /** Page before this row: its ts, and its uuid so rows sharing that ts are not skipped. */
      beforeTs?: string;
      beforeUuid?: string;
    }
  | { id: number; type: "search"; q: string; limit: number }
  | {
      id: number;
      type: "send";
      sessionId: string;
      text: string;
      override?: boolean;
      allowSecret?: boolean;
    }
  /** The owner's answer to a decision (src/mail); `replace` changes an answer already stored. */
  | { id: number; type: "answer"; decisionId: string; text: string; replace?: boolean }
  | { id: number; type: "startDaemon" }
  | { id: number; type: "ping" }
  | { type: "shutdown" };

/** A session as the tray gets it: no transcript path (the tray never reads transcripts). */
export type BridgeSession = Omit<SessionView, "transcriptPath">;

export interface BridgeSnapshot extends Omit<HubSnapshot, "sessions"> {
  sessions: BridgeSession[];
}

export type ParseErrorCode = "bad-json" | "too-long" | "unknown-type" | "bad-request";

/** `id` is present when the line carried a valid one, so the caller can still reply to it. */
export interface ParseError {
  error: ParseErrorCode;
  id?: number;
}

export interface SendEvent {
  id: number;
  sessionId: string;
  state: SendState;
  detail: string | null;
}

export interface ReplyError {
  code: string;
  message: string;
  sendId?: number;
}

export type BridgeEvent =
  | { type: "hello"; protocol: typeof PROTOCOL; version: string }
  | { type: "snapshot"; seq: number; snapshot: BridgeSnapshot }
  /** Sent instead of a snapshot when nothing changed for HEARTBEAT_MS; `seq` is the current one's. */
  | { type: "heartbeat"; seq: number }
  | { type: "send"; send: SendEvent }
  | { type: "error"; code: string; message: string }
  | { type: "reply"; id: number; ok: true; result: unknown }
  | { type: "reply"; id: number; ok: false; error: ReplyError };

const TYPES_WITH_ID = new Set(["history", "search", "send", "answer", "startDaemon", "ping"]);

const isId = (v: unknown): v is number => Number.isSafeInteger(v) && (v as number) >= 0;
const isLimit = (v: unknown): v is number => Number.isSafeInteger(v) && (v as number) >= 1;
const isStr = (v: unknown): v is string => typeof v === "string";

/** Parses one request line. Never throws. */
export function parseRequest(line: string): Request | ParseError {
  if (Buffer.byteLength(line, "utf8") > MAX_LINE_BYTES) return { error: "too-long" };
  let v: unknown;
  try {
    v = JSON.parse(line);
  } catch {
    return { error: "bad-json" };
  }
  if (v === null || typeof v !== "object" || Array.isArray(v)) return { error: "bad-json" };
  const o = v as Record<string, unknown>;
  const withId = (error: ParseErrorCode): ParseError =>
    isId(o.id) ? { error, id: o.id } : { error };

  if (o.type === "shutdown") return { type: "shutdown" };
  if (!isStr(o.type) || !TYPES_WITH_ID.has(o.type)) return withId("unknown-type");
  if (!isId(o.id)) return { error: "bad-request" };
  const id = o.id;
  const bad = withId("bad-request");

  switch (o.type) {
    case "history": {
      if (!isStr(o.sessionId) || !isLimit(o.limit)) return bad;
      if (o.beforeTs !== undefined && !isStr(o.beforeTs)) return bad;
      // beforeUuid names a row: it needs that row's ts.
      if (o.beforeUuid !== undefined && (!isStr(o.beforeUuid) || o.beforeTs === undefined)) {
        return bad;
      }
      const req: Request = {
        id,
        type: "history",
        sessionId: o.sessionId,
        limit: Math.min(o.limit, MAX_LIMIT),
      };
      if (o.beforeTs !== undefined) req.beforeTs = o.beforeTs;
      if (o.beforeUuid !== undefined) req.beforeUuid = o.beforeUuid;
      return req;
    }
    case "search":
      if (!isStr(o.q) || !isLimit(o.limit)) return bad;
      return { id, type: "search", q: o.q, limit: Math.min(o.limit, MAX_LIMIT) };
    case "send": {
      if (!isStr(o.sessionId) || !isStr(o.text)) return bad;
      if (o.override !== undefined && typeof o.override !== "boolean") return bad;
      if (o.allowSecret !== undefined && typeof o.allowSecret !== "boolean") return bad;
      const req: Request = { id, type: "send", sessionId: o.sessionId, text: o.text };
      if (o.override !== undefined) req.override = o.override;
      if (o.allowSecret !== undefined) req.allowSecret = o.allowSecret;
      return req;
    }
    case "answer": {
      if (!isStr(o.decisionId) || !isStr(o.text)) return bad;
      if (o.replace !== undefined && typeof o.replace !== "boolean") return bad;
      const req: Request = { id, type: "answer", decisionId: o.decisionId, text: o.text };
      if (o.replace !== undefined) req.replace = o.replace;
      return req;
    }
    case "startDaemon":
      return { id, type: "startDaemon" };
    default:
      return { id, type: "ping" };
  }
}

/**
 * One event as one line, without the terminator. JSON.stringify escapes every control character,
 * so a `\n` or `\r` inside a value never splits the line.
 */
export function encode(e: BridgeEvent): string {
  return JSON.stringify(e);
}

const LF = 0x0a;
const CR = 0x0d;

/**
 * Splits a byte stream into lines (LF, an optional CR before it dropped). Holds at most
 * MAX_LINE_BYTES + 1 bytes of one line: an oversized line is yielded cut to that length, so it
 * parses as too-long, and the rest of it up to the next LF is discarded.
 */
export async function* readLines(source: AsyncIterable<Uint8Array>): AsyncGenerator<string> {
  const decoder = new TextDecoder();
  const cap = MAX_LINE_BYTES + 1;
  let parts: Uint8Array[] = [];
  let size = 0;
  let skipping = false;

  const take = (): string => {
    const buf = Buffer.concat(parts, size);
    parts = [];
    size = 0;
    const end = buf.length > 0 && buf[buf.length - 1] === CR ? buf.length - 1 : buf.length;
    return decoder.decode(buf.subarray(0, end));
  };

  const add = (bytes: Uint8Array): boolean => {
    if (skipping) return false;
    const room = cap - size;
    if (bytes.length >= room) {
      parts.push(bytes.subarray(0, room));
      size = cap;
      skipping = true;
      return true;
    }
    parts.push(bytes);
    size += bytes.length;
    return false;
  };

  for await (const chunk of source) {
    let start = 0;
    for (;;) {
      const nl = chunk.indexOf(LF, start);
      const piece = chunk.subarray(start, nl === -1 ? chunk.length : nl);
      const overflowed = add(piece);
      if (overflowed) yield take();
      if (nl === -1) break;
      if (skipping) skipping = false;
      else yield take();
      start = nl + 1;
    }
  }
  if (!skipping && size > 0) yield take();
}
