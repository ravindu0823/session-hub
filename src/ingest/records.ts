export type Classified =
  | { kind: "owner-prompt"; uuid: string; ts: string; text: string }
  | {
      kind: "peer-message";
      uuid: string;
      ts: string;
      fromName: string;
      fromMode: string;
      body: string;
    }
  | { kind: "assistant-text"; uuid: string; ts: string; text: string }
  | { kind: "held-notice"; uuid: string; ts: string; peerName: string }
  | { kind: "permission-mode"; ts: string; mode: string }
  /** `type` is set only for reason "unrecognized-type" (a record type this build does not know). */
  | { kind: "ignored"; reason: string; type?: string }
  /** A record type the hub parses (user, assistant, queued_command attachment) changed shape. */
  | { kind: "unknown"; type: string; version?: string };

type Obj = Record<string, unknown>;

// Record types whose content must never leave this function.
const NEVER_STORE = new Set([
  "api-request",
  "api-request-blob",
  "api-request-shape",
  "file-history-snapshot",
  "file-history-delta",
]);

// Known record types the hub does not need.
const KNOWN_IGNORED = new Set([
  "queue-operation",
  "last-prompt",
  "mode",
  "ai-title",
  "tools-used",
  "atis-latch",
  "session-owner",
  "pr-link",
  "frame-link",
  "cost-state",
  "artifact-autoreact-ledger",
  "artifact-comment-monitor",
]);

const HELD_PREFIX = "Held peer message";

function isObj(v: unknown): v is Obj {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

function str(v: unknown): string {
  return typeof v === "string" ? v : "";
}

function ignored(reason: string): Classified {
  return { kind: "ignored", reason };
}

/** Shape change of a parsed record type, for the "format changed (version X)" banner. */
function unknown(r: Obj, type: string): Classified {
  return typeof r.version === "string"
    ? { kind: "unknown", type, version: r.version }
    : { kind: "unknown", type };
}

/** The message object when its content is a string or an array; null when the shape changed. */
function messageOf(r: Obj): Obj | null {
  const msg = r.message;
  if (!isObj(msg)) return null;
  return typeof msg.content === "string" || Array.isArray(msg.content) ? msg : null;
}

/** Joined text of `text` blocks, or null when a tool_result block is present. */
function userText(content: unknown): string | null {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  const parts: string[] = [];
  for (const b of content) {
    if (!isObj(b)) continue;
    if (b.type === "tool_result") return null;
    if (b.type === "text" && typeof b.text === "string") parts.push(b.text);
  }
  return parts.join("\n");
}

function assistantText(content: unknown): string {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  const parts: string[] = [];
  for (const b of content) {
    if (isObj(b) && b.type === "text" && typeof b.text === "string") parts.push(b.text);
  }
  return parts.join("\n");
}

function peerName(content: string): string {
  const m = /peer claims name: ([^);]+)/.exec(content);
  return m?.[1]?.trim() ?? "";
}

const WRAP_OPEN = /<cross-session-message\b([^>]*)>/;
const WRAP_CLOSE = "</cross-session-message>";

function attr(attrs: string, name: string): string {
  return new RegExp(`\\b${name}="([^"]*)"`).exec(attrs)?.[1] ?? "";
}

/**
 * The body of an idle-session peer delivery: the text between the first `<cross-session-message …>`
 * and the last `</cross-session-message>` of the record's content, one line break trimmed at each
 * end. Sender name and mode come from the tag's from-name and from-mode.
 */
function idlePeer(r: Obj, uuid: string, ts: string): Classified {
  const msg = messageOf(r);
  if (msg === null) return unknown(r, "user");
  const text = userText(msg.content);
  if (text === null) return ignored("tool-result");
  const open = WRAP_OPEN.exec(text);
  const end = text.lastIndexOf(WRAP_CLOSE);
  if (!open || end < open.index + open[0].length) return ignored("peer-unwrapped");
  const body = text
    .slice(open.index + open[0].length, end)
    .replace(/^\r?\n/, "")
    .replace(/\r?\n$/, "");
  const attrs = open[1] ?? "";
  return {
    kind: "peer-message",
    uuid,
    ts,
    fromName: attr(attrs, "from-name"),
    fromMode: attr(attrs, "from-mode"),
    body,
  };
}

function fromRecord(r: Obj, type: string): Classified {
  const uuid = str(r.uuid);
  const ts = str(r.timestamp);
  switch (type) {
    case "user": {
      if (r.isSidechain === true) return ignored("sidechain");
      const origin = r.origin;
      // A peer message to an idle session starts its turn as a meta user record (a busy session
      // gets a queued_command attachment instead): a peer message, never an owner prompt.
      if (isObj(origin) && origin.kind === "peer") return idlePeer(r, uuid, ts);
      if (r.isMeta === true) return ignored("meta");
      if (origin !== undefined && (!isObj(origin) || origin.kind !== "human")) {
        return ignored("non-human-origin");
      }
      const msg = messageOf(r);
      if (msg === null) return unknown(r, type);
      const text = userText(msg.content);
      if (text === null) return ignored("tool-result");
      if (text === "") return ignored("empty");
      return { kind: "owner-prompt", uuid, ts, text };
    }
    case "assistant": {
      if (r.isSidechain === true) return ignored("sidechain");
      const msg = messageOf(r);
      if (msg === null) return unknown(r, type);
      const text = assistantText(msg.content);
      if (text === "") return ignored("empty");
      return { kind: "assistant-text", uuid, ts, text };
    }
    case "attachment": {
      const a = r.attachment;
      if (!isObj(a) || a.type !== "queued_command") return ignored("attachment");
      const o = a.origin;
      if (o === undefined) return ignored("attachment");
      if (!isObj(o)) return unknown(r, type);
      // An owner message typed while the session was busy is queued with a human origin: the same
      // owner prompt as a typed turn, so History and answered-item checks see it.
      if (o.kind === "human" && a.humanTurn === true) {
        const text = a.prompt === undefined ? "" : userText(a.prompt);
        if (text === null) return ignored("tool-result");
        if (text === "") return ignored("empty");
        return { kind: "owner-prompt", uuid, ts, text };
      }
      if (o.kind !== "peer") return ignored("attachment");
      return {
        kind: "peer-message",
        uuid,
        ts,
        fromName: str(o.name),
        fromMode: str(o.fromMode),
        body: str(o.body),
      };
    }
    case "system": {
      const content = r.content;
      if (typeof content === "string" && content.startsWith(HELD_PREFIX)) {
        return { kind: "held-notice", uuid, ts, peerName: peerName(content) };
      }
      return ignored("system");
    }
    case "permission-mode": {
      const mode = str(r.permissionMode);
      if (mode === "") return ignored("empty");
      return { kind: "permission-mode", ts: "", mode };
    }
    default:
      // Claude Code adds metadata record types often; a new type is not a format change.
      return { kind: "ignored", reason: "unrecognized-type", type };
  }
}

/** Classify one transcript JSONL line. Never throws. */
export function classify(line: string): Classified {
  let rec: unknown;
  try {
    rec = JSON.parse(line);
  } catch {
    return ignored("bad-json");
  }
  if (!isObj(rec)) return ignored("bad-json");
  const type = str(rec.type);
  if (NEVER_STORE.has(type)) return ignored("never-store");
  if (KNOWN_IGNORED.has(type)) return ignored("not-needed");
  return fromRecord(rec, type);
}
