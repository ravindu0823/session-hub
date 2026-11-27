import { win32 } from "node:path";

/**
 * A role's tool policy (`guard` in roles.json): what `hub guard` blocks for that role's sessions.
 * Names are exact, or end in `*` to match a prefix; no regular expressions.
 */
export interface RoleGuard {
  /** Tool names to block (e.g. `mcp__browser*`). */
  denyTools: string[];
  /** Agent types to block on the Agent (or Task) tool (e.g. `Explore`, `ops-*`). */
  denyAgents: string[];
  /** Folders the file-writing tools may write under; empty means no limit. */
  writeOnlyUnder: string[];
  /** The text shown to the session when a call is blocked. */
  reason: string;
}

/** The parts of a PreToolUse call the guard looks at; nothing else is read. */
export interface GuardCall {
  tool: string;
  subagentType: string | null;
  path: string | null;
}

const AGENT_TOOLS: ReadonlySet<string> = new Set(["Agent", "Task"]);
const WRITE_TOOLS: ReadonlySet<string> = new Set(["Edit", "Write", "MultiEdit", "NotebookEdit"]);

export function matchesName(pattern: string, name: string): boolean {
  return pattern.endsWith("*") ? name.startsWith(pattern.slice(0, -1)) : name === pattern;
}

/** The folder as compared: resolved, backslashes, lower case, no trailing separator. */
function folderKey(p: string): string {
  return win32
    .resolve(p)
    .replace(/[\\/]+$/, "")
    .toLowerCase();
}

/** True when `path` is an absolute path inside (or equal to) one of `folders`. */
export function insideAny(path: string, folders: readonly string[]): boolean {
  if (!win32.isAbsolute(path) || /^[\\/](?![\\/])/.test(path)) return false; // "\x" has no drive
  const p = folderKey(path);
  return folders.some((f) => {
    const k = folderKey(f);
    return p === k || p.startsWith(`${k}\\`);
  });
}

/** Why the call is blocked under `guard`, or null when it is allowed. */
export function blockReason(guard: RoleGuard, call: GuardCall): string | null {
  const why = (what: string) => `${guard.reason} (${what})`;
  if (guard.denyTools.some((p) => matchesName(p, call.tool))) return why(`tool ${call.tool}`);
  if (AGENT_TOOLS.has(call.tool)) {
    const t = call.subagentType ?? "general-purpose"; // the Agent tool's default type
    if (guard.denyAgents.some((p) => matchesName(p, t))) return why(`agent type ${t}`);
  }
  if (WRITE_TOOLS.has(call.tool) && guard.writeOnlyUnder.length > 0) {
    if (call.path === null || !insideAny(call.path, guard.writeOnlyUnder)) {
      return why(`${call.tool} outside ${guard.writeOnlyUnder.join(", ")}`);
    }
  }
  return null;
}
