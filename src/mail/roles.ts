/**
 * Roles: stable addresses for mail. A session's role comes from the local roles file
 * (`roles.json` in the data folder, never in the repository) or, when no entry matches, from the
 * session's own name. Role names, profiles and people live only in that local file.
 */

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

export interface RoleEntry {
  role: string;
  /** Profile id the role's sessions run in; null matches any profile. */
  profile: string | null;
  /** Session-name patterns, `*` matches any run of characters; compared without case. */
  sessions: string[];
}

export interface RolesConfig {
  roles: RoleEntry[];
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
  return { role, profile, sessions };
}

/** Parses the roles file; null (no file) gives no roles. Throws RolesConfigError when invalid. */
export function parseRolesConfig(text: string | null): RolesConfig {
  const cfg: RolesConfig = {
    roles: [],
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
