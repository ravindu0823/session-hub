import { isRoleName, parseRolesConfig, roleOfSession } from "../mail/roles.ts";
import { insideClaudeCode } from "../mail/service.ts";
import type { Store } from "../store/db.ts";
import { blockReason, type GuardCall } from "./policy.ts";

/** What `hub guard` needs; the mail CLI's dependencies provide it. */
export interface GuardDeps {
  openStore(): Store | null;
  readRoles(): string | null;
  env: Record<string, string | undefined>;
  now(): number;
  out(line: string): void;
  err(line: string): void;
  readInput(path: string): Promise<string>;
  /** The owner's overrides file (`guard-off.json` in the data folder), or null when there is none. */
  readGuardOff?(): string | null;
  writeGuardOff?(text: string): void;
}

export const GUARD_USAGE = [
  'Per-role tool policy (the role\'s "guard" in roles.json):',
  "  hub guard                  (PreToolUse hook) blocks a call the session's role policy denies",
  "  hub guard status           roles with a policy, and any override",
  "  hub guard off --role <role> --minutes <1-480>   (the owner, outside sessions) pause a policy",
  "  hub guard on --role <role>                      (the owner, outside sessions) end the pause",
].join("\n");

const SESSION_RE = /^[A-Za-z0-9-]{1,100}$/;
const MAX_TOOL_CHARS = 200;
const MAX_PATH_CHARS = 2_000;
const MAX_OFF_MINUTES = 480;

/** The call from the hook input: only these fields are read; nothing is stored or logged. */
function readCall(text: string): (GuardCall & { sid: string }) | null {
  let input: unknown;
  try {
    input = JSON.parse(text);
  } catch {
    return null;
  }
  if (typeof input !== "object" || input === null) return null;
  const o = input as Record<string, unknown>;
  const tool = o.tool_name;
  const sid = o.session_id;
  if (typeof tool !== "string" || tool === "" || tool.length > MAX_TOOL_CHARS) return null;
  if (typeof sid !== "string" || !SESSION_RE.test(sid)) return null;
  const ti =
    typeof o.tool_input === "object" && o.tool_input !== null
      ? (o.tool_input as Record<string, unknown>)
      : {};
  const str = (v: unknown) => (typeof v === "string" && v.length <= MAX_PATH_CHARS ? v : null);
  return {
    sid,
    tool,
    subagentType: str(ti.subagent_type),
    path: str(ti.file_path) ?? str(ti.notebook_path),
  };
}

/** Role -> time the owner's pause ends; anything unreadable counts as no pause. */
function readOff(d: GuardDeps): Map<string, number> {
  const out = new Map<string, number>();
  try {
    const o = JSON.parse(d.readGuardOff?.() ?? "{}") as Record<string, unknown>;
    for (const [role, until] of Object.entries(o)) {
      if (isRoleName(role) && typeof until === "number" && Number.isFinite(until)) {
        out.set(role, until);
      }
    }
  } catch {
    // no pause
  }
  return out;
}

/**
 * `hub guard` as a PreToolUse hook: blocks (exit 2, the reason on stderr, which Claude Code shows
 * to the session) a call that the policy of the session's role denies. Sessions whose role has no
 * policy, unknown sessions, bad input, a missing index and every failure pass (exit 0): the hub
 * being down never stops a session's work.
 */
async function runHook(d: GuardDeps): Promise<number> {
  let store: Store | null = null;
  try {
    const call = readCall(await d.readInput("-"));
    if (call === null) return 0;
    const cfg = parseRolesConfig(d.readRoles());
    if (!cfg.roles.some((r) => r.guard !== null)) return 0; // no policy anywhere: no index read
    store = d.openStore();
    if (store === null) return 0;
    const s = store.getSession(call.sid);
    if (s === null) return 0;
    const r = roleOfSession(cfg, s);
    if ("error" in r) return 0;
    const guard = cfg.roles.find((e) => e.role === r.role)?.guard ?? null;
    if (guard === null) return 0;
    if ((readOff(d).get(r.role) ?? 0) > d.now()) return 0;
    const reason = blockReason(guard, call);
    if (reason === null) return 0;
    d.err(
      `hub guard: blocked for role ${r.role}: ${reason}. ` +
        `Owner override, outside Claude Code: hub guard off --role ${r.role} --minutes 30`,
    );
    return 2;
  } catch (e) {
    d.err(`hub guard: skipped (${e instanceof Error ? e.name : "unknown"})`);
    return 0;
  } finally {
    store?.close();
  }
}

function timeOf(ms: number): string {
  return new Date(ms).toISOString().slice(0, 16).replace("T", " ");
}

/** `hub guard [status | off | on]`. Exit codes: 0 ok (or the hook passed), 1 refused, 2 usage/blocked. */
export async function runGuardCli(args: readonly string[], d: GuardDeps): Promise<number> {
  if (args.length === 0) return runHook(d);
  const [sub, ...rest] = args;
  if (sub === "--help" || sub === "-h") {
    d.out(GUARD_USAGE);
    return 0;
  }
  if (sub === "status" && rest.length === 0) {
    const cfg = parseRolesConfig(d.readRoles());
    const off = readOff(d);
    const guarded = cfg.roles.filter((r) => r.guard !== null);
    for (const r of guarded) {
      const g = r.guard;
      if (g === null) continue;
      const until = off.get(r.role) ?? 0;
      const state = until > d.now() ? `PAUSED until ${timeOf(until)} UTC` : "ON";
      d.out(
        `${r.role}: ${state} (${g.denyTools.length} tools, ${g.denyAgents.length} agent types, ` +
          `${g.writeOnlyUnder.length === 0 ? "writes anywhere" : `writes only under ${g.writeOnlyUnder.join(", ")}`})`,
      );
    }
    if (guarded.length === 0) d.out("no role has a guard policy");
    return 0;
  }
  const role = rest[0] === "--role" ? rest[1] : undefined;
  const isOff = sub === "off" && rest.length === 4 && rest[2] === "--minutes";
  const isOn = sub === "on" && rest.length === 2;
  if ((!isOff && !isOn) || role === undefined || !isRoleName(role)) {
    d.err(GUARD_USAGE);
    return 2;
  }
  // Prevents mistakes only (a session pausing its own guard by accident): any process can unset
  // these variables, so the guard is a guard rail, not a sandbox.
  if (insideClaudeCode(d.env)) {
    d.err("hub guard: refused: a pause is for the owner, outside Claude Code sessions");
    return 1;
  }
  if (d.writeGuardOff === undefined) return 1;
  const off = readOff(d);
  if (isOff) {
    const minutes = Number(rest[3]);
    if (!/^\d{1,3}$/.test(rest[3] ?? "") || minutes < 1 || minutes > MAX_OFF_MINUTES) {
      d.err(GUARD_USAGE);
      return 2;
    }
    off.set(role, d.now() + minutes * 60_000);
  } else {
    off.delete(role);
  }
  for (const [r, until] of off) if (until <= d.now()) off.delete(r);
  d.writeGuardOff(`${JSON.stringify(Object.fromEntries(off))}\n`);
  d.out(
    isOff ? `${role}: guard PAUSED until ${timeOf(off.get(role) ?? 0)} UTC` : `${role}: guard ON`,
  );
  return 0;
}
