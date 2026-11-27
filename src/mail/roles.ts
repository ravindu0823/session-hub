/**
 * Roles: stable addresses for mail. A session's role comes from the local roles file
 * (`roles.json` in the data folder, never in the repository) or, when no entry matches, from the
 * session's own name. Role names, profiles and people live only in that local file.
 */

import type { RoleGuard } from "../guard/policy.ts";

/** Lower-case letters, digits, ".", "_" and "-", starting with a letter or digit; 1 to 64 characters. */
const ROLE_RE = /^[a-z0-9][a-z0-9._-]{0,63}$/;

/** `owner` is the person (tray, or a terminal outside Claude Code); `hub` is Session Hub itself. */
export const OWNER_ROLE = "owner";
export const HUB_ROLE = "hub";
const RESERVED: ReadonlySet<string> = new Set([OWNER_ROLE, HUB_ROLE]);

export const DEFAULT_EXPIRY_MS = 72 * 3_600_000;
export const DEFAULT_ACK_TIMEOUT_MS = 30 * 60_000;
const MAX_EXPIRY_HOURS = 7 * 24;
const MAX_ACK_TIMEOUT_MINUTES = 24 * 60;
const MAX_PATTERN_CHARS = 64;

/** Effort levels a role may name (Claude Code's `--effort`). */
export const EFFORTS: readonly string[] = ["low", "medium", "high", "xhigh", "max"];

/** Model aliases or full `claude-*` ids, each optionally with `[1m]`; nothing a shell could read. */
const MODEL_RE = /^(?:opus|sonnet|haiku|fable|claude-[a-z0-9-]{1,60})(?:\[1m\])?$/;
/** Question-ID family prefix, e.g. `WEB` for `WEB-Q12`: upper case and digits, 1 to 12 characters. */
const PREFIX_RE = /^[A-Z][A-Z0-9]{0,11}$/;
/** Names of MCP servers, plugins and chats: shown, never run. */
const NAME_RE = /^[A-Za-z0-9@._:-]{1,64}$/;
const MAX_NAMES = 50;
const MAX_PATH_CHARS = 260;
/** A tool or agent-type name, optionally ending in `*` (a prefix), or `*` alone. */
const GUARD_NAME_RE = /^(?:[A-Za-z0-9@._:-]{1,100}\*?|\*)$/;
const MAX_GUARD_FOLDERS = 10;
const MAX_REASON_CHARS = 300;

export interface RoleEntry {
  role: string;
  /** Profile id the role's sessions run in; null matches any profile. */
  profile: string | null;
  /** Session-name patterns, `*` matches any run of characters; compared without case. */
  sessions: string[];
  // The role catalogue: how a session of this role is started. Read by launchers through
  // `hub roles --json`; the hub itself only checks and shows it.
  /** Absolute folder the session starts in. */
  folder: string | null;
  /** Absolute path of the start prompt or handover file. */
  brief: string | null;
  model: string | null;
  effort: string | null;
  /** Started through the team gateway. */
  gateway: boolean;
  mcpServers: string[];
  plugins: string[];
  /** Chat nicknames this role watches; one role per chat. */
  telegramChats: string[];
  /** Family prefix of this role's owner-question IDs; unique. */
  questionPrefix: string | null;
  /** Tool policy `hub guard` applies to this role's sessions; null: none. */
  guard: RoleGuard | null;
}

export interface RolesConfig {
  roles: RoleEntry[];
  /** Question prefixes in use by families with no catalogue entry; no role may take them. */
  reservedPrefixes: string[];
  /** How long queued mail waits for its role before it expires. */
  expiryMs: number;
  /** How long delivered mail may stay unacknowledged before its sender is told. */
  ackTimeoutMs: number;
}

export class RolesConfigError extends Error {
  constructor(message: string) {
    super(`roles.json: ${message}`);
    this.name = "RolesConfigError";
  }
}

export function isRoleName(s: string): boolean {
  return ROLE_RE.test(s);
}

export function isReservedRole(s: string): boolean {
  return RESERVED.has(s);
}

export function matchesSessionPattern(pattern: string, name: string): boolean {
  const re = pattern
    .split("*")
    .map((part) => part.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"))
    .join(".*");
  return new RegExp(`^${re}$`, "is").test(name);
}

function isRecord(v: unknown): v is Record<string, unknown> {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}

function positiveNumber(v: unknown, max: number, what: string): number {
  if (typeof v !== "number" || !Number.isFinite(v) || v <= 0 || v > max) {
    throw new RolesConfigError(`${what} must be a number above 0 and at most ${max}`);
  }
  return v;
}

/** An absolute Windows path with no character a launcher's command line could misread. */
function optionalPath(v: unknown, role: string, what: string): string | null {
  if (v === undefined) return null;
  if (
    typeof v !== "string" ||
    v.length > MAX_PATH_CHARS ||
    !/^[A-Za-z]:[\\/]/.test(v) ||
    // biome-ignore lint/suspicious/noControlCharactersInRegex: control characters are what it refuses
    /[;"\u0000-\u001f\u007f]/.test(v)
  ) {
    throw new RolesConfigError(
      `"${role}": ${what} must be an absolute path (C:\\...) without ';', '"' or control characters`,
    );
  }
  return v;
}

function optionalMatch(v: unknown, re: RegExp, role: string, what: string): string | null {
  if (v === undefined) return null;
  if (typeof v !== "string" || !re.test(v)) {
    throw new RolesConfigError(`"${role}": ${what} is not valid`);
  }
  return v;
}

function nameList(v: unknown, role: string, what: string): string[] {
  if (v === undefined) return [];
  if (
    !Array.isArray(v) ||
    v.length > MAX_NAMES ||
    !v.every((n) => typeof n === "string" && NAME_RE.test(n))
  ) {
    throw new RolesConfigError(
      `"${role}": ${what} must be a list of names (letters, digits, @ . _ : -)`,
    );
  }
  return v as string[];
}

function guardNames(v: unknown, role: string, what: string): string[] {
  if (v === undefined) return [];
  if (
    !Array.isArray(v) ||
    v.length > MAX_NAMES ||
    !v.every((n) => typeof n === "string" && GUARD_NAME_RE.test(n))
  ) {
    throw new RolesConfigError(
      `"${role}": guard.${what} must be a list of tool or agent names, each optionally ending in *`,
    );
  }
  return v as string[];
}

/** The role's tool policy for `hub guard`, or null when it has none. */
function parseGuard(v: unknown, role: string): RoleGuard | null {
  if (v === undefined) return null;
  if (!isRecord(v)) throw new RolesConfigError(`"${role}": guard must be an object`);
  const folders = v.writeOnlyUnder ?? [];
  if (!Array.isArray(folders) || folders.length > MAX_GUARD_FOLDERS) {
    throw new RolesConfigError(
      `"${role}": guard.writeOnlyUnder must be a list of up to 10 folders`,
    );
  }
  const reason = v.reason ?? "this session's role does not use this tool: delegate it";
  if (
    typeof reason !== "string" ||
    reason.trim() === "" ||
    reason.length > MAX_REASON_CHARS ||
    // biome-ignore lint/suspicious/noControlCharactersInRegex: control characters are what it refuses
    /[\u0000-\u001f\u007f]/.test(reason)
  ) {
    throw new RolesConfigError(`"${role}": guard.reason must be one line of 1 to 300 characters`);
  }
  return {
    denyTools: guardNames(v.denyTools, role, "denyTools"),
    denyAgents: guardNames(v.denyAgents, role, "denyAgents"),
    writeOnlyUnder: folders.map(
      (f) => optionalPath(f, role, "a guard.writeOnlyUnder folder") ?? "",
    ),
    reason,
  };
}

function parseEntry(v: unknown, seen: Set<string>): RoleEntry {
  if (!isRecord(v)) throw new RolesConfigError("each entry in roles must be an object");
  const role = v.role;
  if (typeof role !== "string" || !isRoleName(role)) {
    throw new RolesConfigError(`"${String(role)}" is not a role name (a-z, 0-9, ".", "_", "-")`);
  }
  if (isReservedRole(role)) throw new RolesConfigError(`"${role}" is reserved`);
  if (seen.has(role)) throw new RolesConfigError(`"${role}" is listed twice`);
  seen.add(role);
  let profile: string | null = null;
  if (v.profile !== undefined) {
    if (typeof v.profile !== "string" || v.profile === "") {
      throw new RolesConfigError(`"${role}": profile must be a non-empty string`);
    }
    profile = v.profile;
  }
  let sessions = [role];
  if (v.sessions !== undefined) {
    const list = v.sessions;
    if (
      !Array.isArray(list) ||
      list.length === 0 ||
      !list.every((p) => typeof p === "string" && p !== "" && p.length <= MAX_PATTERN_CHARS)
    ) {
      throw new RolesConfigError(
        `"${role}": sessions must be a list of 1 to 64 character patterns`,
      );
    }
    sessions = list as string[];
  }
  const effort = optionalMatch(v.effort, /^[a-z]+$/, role, "effort");
  if (effort !== null && !EFFORTS.includes(effort)) {
    throw new RolesConfigError(`"${role}": effort must be one of ${EFFORTS.join(", ")}`);
  }
  if (v.gateway !== undefined && typeof v.gateway !== "boolean") {
    throw new RolesConfigError(`"${role}": gateway must be true or false`);
  }
  return {
    role,
    profile,
    sessions,
    folder: optionalPath(v.folder, role, "folder"),
    brief: optionalPath(v.brief, role, "brief"),
    model: optionalMatch(v.model, MODEL_RE, role, "model"),
    effort,
    gateway: v.gateway === true,
    mcpServers: nameList(v.mcpServers, role, "mcpServers"),
    plugins: nameList(v.plugins, role, "plugins"),
    telegramChats: nameList(v.telegramChats, role, "telegramChats"),
    questionPrefix: optionalMatch(v.questionPrefix, PREFIX_RE, role, "questionPrefix"),
    guard: parseGuard(v.guard, role),
  };
}

/** Every question prefix names one family: the reserved ones and each role's own. */
function checkPrefixes(cfg: RolesConfig): void {
  const owner = new Map<string, string>();
  for (const p of cfg.reservedPrefixes) {
    if (owner.has(p)) throw new RolesConfigError(`reserved prefix ${p} is listed twice`);
    owner.set(p, "reservedPrefixes");
  }
  for (const e of cfg.roles) {
    if (e.questionPrefix === null) continue;
    const other = owner.get(e.questionPrefix);
    if (other !== undefined) {
      throw new RolesConfigError(
        `"${e.role}": question prefix ${e.questionPrefix} is already used by ${other}`,
      );
    }
    owner.set(e.questionPrefix, `role "${e.role}"`);
  }
}

/** Parses the roles file; null (no file) gives no roles. Throws RolesConfigError when invalid. */
export function parseRolesConfig(text: string | null): RolesConfig {
  const cfg: RolesConfig = {
    roles: [],
    reservedPrefixes: [],
    expiryMs: DEFAULT_EXPIRY_MS,
    ackTimeoutMs: DEFAULT_ACK_TIMEOUT_MS,
  };
  if (text === null) return cfg;
  let json: unknown;
  try {
    json = JSON.parse(text.replace(/^﻿/, ""));
  } catch {
    throw new RolesConfigError("not valid JSON");
  }
  if (!isRecord(json)) throw new RolesConfigError("must be a JSON object");
  if (json.roles !== undefined) {
    if (!Array.isArray(json.roles)) throw new RolesConfigError("roles must be a list");
    const seen = new Set<string>();
    cfg.roles = json.roles.map((e) => parseEntry(e, seen));
  }
  if (json.reservedPrefixes !== undefined) {
    const list = json.reservedPrefixes;
    if (!Array.isArray(list) || !list.every((p) => typeof p === "string" && PREFIX_RE.test(p))) {
      throw new RolesConfigError("reservedPrefixes must be a list of prefixes (A-Z, 0-9)");
    }
    cfg.reservedPrefixes = list as string[];
  }
  checkPrefixes(cfg);
  if (json.mail !== undefined) {
    if (!isRecord(json.mail)) throw new RolesConfigError("mail must be an object");
    if (json.mail.expiryHours !== undefined) {
      cfg.expiryMs =
        positiveNumber(json.mail.expiryHours, MAX_EXPIRY_HOURS, "mail.expiryHours") * 3_600_000;
    }
    if (json.mail.ackTimeoutMinutes !== undefined) {
      cfg.ackTimeoutMs =
        positiveNumber(
          json.mail.ackTimeoutMinutes,
          MAX_ACK_TIMEOUT_MINUTES,
          "mail.ackTimeoutMinutes",
        ) * 60_000;
    }
  }
  return cfg;
}

export type RoleResult = { role: string } | { error: string };

/**
 * The role of a session: the configured entry whose profile and patterns match it, else its own
 * name lower-cased, when that is a role name that no configured entry claims.
 */
export function roleOfSession(cfg: RolesConfig, s: { name: string; profile: string }): RoleResult {
  const hits = cfg.roles.filter(
    (e) =>
      (e.profile === null || e.profile === s.profile) &&
      e.sessions.some((p) => matchesSessionPattern(p, s.name)),
  );
  if (hits.length > 1) {
    return {
      error: `session "${s.name}" matches more than one role in roles.json (${hits.map((h) => h.role).join(", ")})`,
    };
  }
  if (hits[0]) return { role: hits[0].role };
  const own = s.name.toLowerCase();
  if (!isRoleName(own)) {
    return {
      error: `session name "${s.name}" is not a role name; start the session with a role name (claude -n <role>) or add it to roles.json`,
    };
  }
  if (isReservedRole(own)) return { error: `session name "${s.name}" is a reserved role` };
  const claimed = cfg.roles.find((e) => e.role === own);
  if (claimed) {
    return {
      error: `role "${own}" belongs to sessions in profile ${claimed.profile ?? "(any)"} matching ${claimed.sessions.join(", ")}`,
    };
  }
  return { role: own };
}

/** Every role mail may be addressed to: configured ones, the owner, and those of indexed sessions. */
export function knownRoles(
  cfg: RolesConfig,
  sessions: readonly { name: string; profile: string }[],
): Set<string> {
  const out = new Set<string>([OWNER_ROLE, ...cfg.roles.map((e) => e.role)]);
  for (const s of sessions) {
    const r = roleOfSession(cfg, s);
    if ("role" in r) out.add(r.role);
  }
  return out;
}

/**
 * For the daemon: reads the roles file on every call. An invalid or unreadable file gives the default
 * timings and no roles, and `warn("roles_invalid")` runs once until the file is valid (or gone) again.
 * The warning carries no file content.
 */
export function rolesLoader(
  read: () => string | null,
  warn: (event: string) => void,
): () => RolesConfig {
  let warned = false;
  return () => {
    try {
      const cfg = parseRolesConfig(read());
      warned = false;
      return cfg;
    } catch {
      if (!warned) warn("roles_invalid");
      warned = true;
      return parseRolesConfig(null);
    }
  };
}
