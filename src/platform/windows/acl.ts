import { mkdir, stat } from "node:fs/promises";
import { run, runChecked } from "./exec";
import { sysbin } from "./sysbin";

const SYSTEM_NAME = "nt authority\\system";
const SYSTEM_SID = "s-1-5-18";

export interface Identity {
  /** `DOMAIN\user`, lower-case. */
  name: string;
  /** `S-1-5-21-...`, lower-case. */
  sid: string;
}

let identity: Promise<Identity> | undefined;

/** Current user's name and SID, resolved once via `whoami /user`. */
export function currentIdentity(): Promise<Identity> {
  identity ??= (async () => {
    const r = await runChecked([sysbin.whoami(), "/user", "/fo", "csv", "/nh"]);
    const m = /^"([^"]+)","(S-[0-9-]+)"/.exec(r.stdout.trim());
    if (!m?.[1] || !m[2]) throw new Error("whoami returned no SID");
    return { name: m[1].toLowerCase(), sid: m[2].toLowerCase() };
  })();
  identity.catch(() => {
    identity = undefined;
  });
  return identity;
}

/** Principals named in `icacls <dir>` output (ACE lines up to the first blank line). */
export function parsePrincipals(output: string, dir: string): string[] {
  const out: string[] = [];
  for (const raw of output.split(/\r?\n/)) {
    let line = raw.trim();
    if (line === "") break;
    if (line.toLowerCase().startsWith(dir.toLowerCase())) line = line.slice(dir.length).trim();
    const m = /^(.+?):\(/.exec(line);
    if (m?.[1]) out.push(m[1].trim());
  }
  return out;
}

/** Principals that are neither the current user (name or SID) nor SYSTEM (by English name or SID). */
export function blockingPrincipals(principals: string[], me: Identity): string[] {
  return principals.filter((p) => {
    const l = p.toLowerCase();
    return l !== me.name && l !== me.sid && l !== SYSTEM_NAME && l !== SYSTEM_SID;
  });
}

/** null: directory missing or unreadable. [] : private. Otherwise the blocking principals. */
async function inspect(dir: string): Promise<string[] | null> {
  try {
    if (!(await stat(dir)).isDirectory()) return null;
  } catch {
    return null;
  }
  const r = await run([sysbin.icacls(), dir]);
  if (r.code !== 0) return null;
  const principals = parsePrincipals(r.stdout, dir);
  if (principals.length === 0) return null;
  return blockingPrincipals(principals, await currentIdentity());
}

export async function isPrivateDir(dir: string): Promise<boolean> {
  const blockers = await inspect(dir);
  return blockers !== null && blockers.length === 0;
}

export async function ensurePrivateDir(dir: string): Promise<void> {
  await mkdir(dir, { recursive: true });
  const me = await currentIdentity();
  await runChecked([
    sysbin.icacls(),
    dir,
    "/inheritance:r",
    "/grant:r",
    `*${me.sid.toUpperCase()}:(OI)(CI)F`,
    "*S-1-5-18:(OI)(CI)F",
  ]);
  // /inheritance:r drops inherited entries and /grant:r replaces only the two grants above, so an
  // explicit entry for anyone else survives (an elevated admin's new folders can carry one for
  // BUILTIN\Administrators). Remove each remaining principal by the name icacls printed, then verify.
  let blockers = await inspect(dir);
  if (blockers !== null && blockers.length > 0) {
    for (const principal of blockers)
      await runChecked([sysbin.icacls(), dir, "/remove", principal]);
    blockers = await inspect(dir);
  }
  if (blockers === null) throw new Error("directory is not private: icacls could not verify it");
  if (blockers.length > 0) {
    throw new Error(`directory is not private; blocking principals: ${blockers.join(", ")}`);
  }
}
