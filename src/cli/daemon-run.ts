import { mkdirSync, readFileSync, rmSync } from "node:fs";
import { join } from "node:path";
import { DAEMON_STOPPED_FILE, EXPIRE_EVERY_MS } from "../config.ts";
import { type DaemonDeps, runDaemon as realRunDaemon } from "../daemon/collector.ts";
import { type DaemonIdentity, LOCK_FILE, type LockInfo, parseLock } from "../daemon/lock.ts";
import { createLogger, errFields, type Logger } from "../daemon/log.ts";
import type { ClaudeCli } from "../discovery/agents.ts";
import type { Classified } from "../ingest/records.ts";
import type { UpkeepResult } from "../mail/service.ts";
import type { Platform } from "../platform/types.ts";
import { Store } from "../store/db.ts";
import type { CliPaths, ForegroundResult } from "./commands.ts";

const SIGNALS = ["SIGINT", "SIGTERM", "SIGBREAK"] as const;
/** Process-level failures: logged, then the daemon stops cleanly instead of dying silently. */
const FATAL = ["uncaughtException", "unhandledRejection"] as const;
type Sig = (typeof SIGNALS)[number] | (typeof FATAL)[number];

export interface DaemonRunDeps {
  platform: Platform;
  paths: CliPaths;
  profiles: readonly { id: string; dir: string }[];
  cli: ClaudeCli;
  now: () => number;
  /** Classifies the process daemon.lock names (see DaemonIdentity). */
  daemonIdentity(lock: LockInfo): DaemonIdentity;
  /** Builds the per-record hook (send delivery check) for this store. */
  onRecords(
    store: Store,
    now: () => number,
    log: Logger,
  ): (sessionId: string, records: Classified[]) => void;
  /** Expires overdue sends; returns how many. */
  expireSends(store: Store, now: () => number): number;
  /** Mail upkeep (requeue, expire, flag); absent in tests that do not need it. */
  mailUpkeep?(store: Store, now: () => number, log: Logger): UpkeepResult;
  runDaemon?: (deps: DaemonDeps) => Promise<void>;
  /** Signal and process-failure events (default: process). */
  signals?: {
    on(sig: Sig, fn: (e?: unknown) => void): void;
    off(sig: Sig, fn: (e?: unknown) => void): void;
  };
}

/**
 * One upkeep pass, every EXPIRE_EVERY_MS: overdue sends expire, then the mail upkeep runs. Each step
 * is logged when it changed something; a failing step is logged and does not stop the other.
 */
export function upkeepTick(
  store: Store,
  deps: Pick<DaemonRunDeps, "now" | "expireSends" | "mailUpkeep">,
  log: Logger,
): void {
  try {
    const n = deps.expireSends(store, deps.now);
    if (n > 0) log.info("sends_expired", { count: n });
  } catch (e) {
    log.warn("expire_failed", errFields(e));
  }
  if (deps.mailUpkeep === undefined) return;
  try {
    const r = deps.mailUpkeep(store, deps.now, log);
    if (r.expired + r.requeued + r.flagged > 0) {
      log.info("mail_upkeep", { expired: r.expired, requeued: r.requeued, flagged: r.flagged });
    }
  } catch (e) {
    log.warn("mail_upkeep_failed", errFields(e));
  }
}

/** True unless the lock is missing, garbage, or names a "foreign" pid (dead or another program). */
function lockHeld(runDir: string, identify: (lock: LockInfo) => DaemonIdentity): boolean {
  let text: string;
  try {
    text = readFileSync(join(runDir, LOCK_FILE), "utf8").trim();
  } catch {
    return false;
  }
  const lock = parseLock(text);
  return lock !== null && identify(lock) !== "foreign";
}

/**
 * `hub daemon run`: the collector in this process until SIGINT/SIGTERM/SIGBREAK. The lock itself is
 * taken inside runDaemon; a lock held by our daemon or by a pid that cannot be verified, seen before
 * or during start, means "already-running". Only a lock naming a dead pid or another program is stale.
 */
export async function daemonRun(deps: DaemonRunDeps): Promise<ForegroundResult> {
  const p = deps.paths;
  if (lockHeld(p.runDir, deps.daemonIdentity)) return "already-running";

  await deps.platform.ensurePrivateDir(p.dataDir);
  mkdirSync(p.runDir, { recursive: true });
  mkdirSync(p.relayDir, { recursive: true });
  // A running daemon (the login start included) ends any earlier deliberate stop.
  rmSync(join(p.runDir, DAEMON_STOPPED_FILE), { force: true });
  // The logger comes first, so a store that cannot open still leaves a line in daemon.log.
  const log = createLogger(p.runDir);
  let store: Store;
  try {
    store = Store.open(p.dbPath);
  } catch (e) {
    log.error("store_open_failed", errFields(e));
    throw e;
  }
  const ac = new AbortController();
  const stop = () => ac.abort();
  const fatal = (e?: unknown) => {
    log.error("uncaught", errFields(e));
    ac.abort();
  };
  const signals = deps.signals ?? (process as unknown as NonNullable<DaemonRunDeps["signals"]>);
  for (const s of SIGNALS) signals.on(s, stop);
  for (const s of FATAL) signals.on(s, fatal);
  const expiry = setInterval(() => upkeepTick(store, deps, log), EXPIRE_EVERY_MS);
  try {
    await (deps.runDaemon ?? realRunDaemon)({
      cli: deps.cli,
      store,
      platform: deps.platform,
      now: deps.now,
      profiles: deps.profiles,
      dataDir: p.dataDir,
      relayDir: p.relayDir,
      signal: ac.signal,
      onRecords: deps.onRecords(store, deps.now, log),
      daemonIdentity: deps.daemonIdentity,
      log,
    });
    return "stopped";
  } catch (e) {
    if (e instanceof Error && e.message === "daemon already running") return "already-running";
    log.error("daemon_failed", errFields(e));
    throw e;
  } finally {
    clearInterval(expiry);
    for (const s of SIGNALS) signals.off(s, stop);
    for (const s of FATAL) signals.off(s, fatal);
    store.close();
  }
}
