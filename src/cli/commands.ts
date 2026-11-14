import { createHash } from "node:crypto";
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  renameSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { basename, dirname, join, resolve } from "node:path";
import { DAEMON_STOPPED_FILE } from "../config.ts";
import {
  type DaemonIdentity,
  identifyDaemon,
  isKillableImage,
  LOCK_FILE,
  type LockInfo,
  parseLock,
} from "../daemon/lock.ts";
import type { Platform } from "../platform/types.ts";
import {
  describeBuild,
  formatManifest,
  MANIFEST_FILE,
  type Manifest,
  parseManifest,
} from "./manifest.ts";

export interface CliPaths {
  dataDir: string;
  dbPath: string;
  runDir: string;
  relayDir: string;
  binDir: string;
  installedExe: string;
  /** The console-less daemon exe; autostart and `daemon start` run it with no arguments. */
  installedDaemonExe: string;
  /** The tray app; autostart runs it with no arguments. */
  installedTrayExe: string;
  /** Written by the tray after its single-instance check, deleted on a clean exit. */
  trayPidFile: string;
  shimDir: string;
  shimPath: string;
}

/** Pure: every path the CLI touches, from the data dir and %USERPROFILE%. */
export function cliPaths(opts: { dataDir: string; userProfile: string }): CliPaths {
  const { dataDir } = opts;
  const binDir = join(dataDir, "bin");
  const runDir = join(dataDir, "run");
  const shimDir = join(opts.userProfile, ".local", "bin");
  return {
    dataDir,
    dbPath: join(dataDir, "hub.db"),
    runDir,
    relayDir: join(dataDir, "relay"),
    binDir,
    installedExe: join(binDir, "hub.exe"),
    installedDaemonExe: join(binDir, DAEMON_EXE),
    installedTrayExe: join(binDir, TRAY_EXE),
    trayPidFile: join(runDir, "tray.pid"),
    shimDir,
    shimPath: join(shimDir, "hub.cmd"),
  };
}

/** Built next to hub.exe with no console window; runs `daemon run` when started with no arguments. */
export const DAEMON_EXE = "hubd.exe";

/** The tray app, built by `bun run build` from gui/SessionHub.Tray into dist next to hub.exe. */
export const TRAY_EXE = "SessionHubTray.exe";

/** Exact content of our `hub.cmd`; anything else at that path is someone else's file. */
export const SHIM_CONTENT = '@"%LOCALAPPDATA%\\SessionHub\\bin\\hub.exe" %*\r\n';

export const USAGE = [
  "Usage:",
  "  hub                     open the TUI (starts the daemon if it is not running)",
  "  hub daemon run          run the daemon in this console",
  "  hub daemon start        start the daemon hidden in the background",
  "  hub daemon stop         stop the daemon",
  "  hub daemon status       RUNNING pid=<n> (exit 0) or STOPPED (exit 3)",
  "  hub install [--allow-unreviewed]",
  "                          install dist\\*.exe (hub, daemon, tray) and start both at login;",
  "                          refuses a build from uncommitted changes without --allow-unreviewed",
  "  hub uninstall [--keep-data]",
  "                          remove the install and the data; --keep-data keeps the data",
  "  hub bridge              (internal) data link for the tray app over stdin/stdout",
  "  hub --version           print the version",
  "  hub --help              print this help",
].join("\n");

export interface CliFs {
  exists(path: string): boolean;
  /** Null when the file cannot be read. */
  readText(path: string): string | null;
  writeText(path: string, text: string): void;
  mkdirp(path: string): void;
  copy(src: string, dst: string): void;
  rename(from: string, to: string): void;
  /** Recursive and forced: a missing path is not an error. */
  remove(path: string): void;
  /** Lower-case SHA-256 hex of the file's bytes; throws when it cannot be read. */
  sha256(path: string): string;
}

export const nodeFs: CliFs = {
  exists: (p) => existsSync(p),
  readText: (p) => {
    try {
      return readFileSync(p, "utf8");
    } catch {
      return null;
    }
  },
  writeText: (p, t) => writeFileSync(p, t),
  mkdirp: (p) => {
    mkdirSync(p, { recursive: true });
  },
  copy: (s, d) => copyFileSync(s, d),
  rename: (f, t) => renameSync(f, t),
  remove: (p) => rmSync(p, { recursive: true, force: true }),
  sha256: (p) => createHash("sha256").update(readFileSync(p)).digest("hex"),
};

export type ForegroundResult = "already-running" | "stopped";

/** What `hub bridge` gets from the CLI: the same daemon checks `hub daemon status|start` use. */
export { DAEMON_STOPPED_FILE };

/** Before a deliberate stop: the tray's bridge must not start the daemon again. */
function markStopped(d: CliDeps): void {
  d.fs.mkdirp(d.paths.runDir);
  d.fs.writeText(join(d.paths.runDir, DAEMON_STOPPED_FILE), "");
}

export interface BridgeLink {
  daemonRunning(): boolean;
  /** True after `hub daemon stop` until the next start: the bridge then never restarts it. */
  stopRequested(): boolean;
  /** Starts the daemon hidden if it is stopped; true when it runs afterwards. */
  startDaemon(): Promise<boolean>;
}

export interface CliDeps {
  platform: Platform;
  paths: CliPaths;
  version: string;
  /** Path of the running executable (hub.exe when compiled, bun.exe in dev). */
  exePath: string;
  /** True when running as the compiled hub.exe. */
  compiled: boolean;
  /**
   * argv that runs the daemon in the foreground: `[<dir>\hubd.exe]` when it sits next to the compiled
   * exe, else `[hub.exe, "daemon", "run"]` or, in dev, `[bun.exe, <abs>/src/cli.ts, "daemon", "run"]`.
   */
  daemonArgv: string[];
  fs: CliFs;
  out(line: string): void;
  err(line: string): void;
  /**
   * Detached, stdio ignored, unref'd; hidden unless `gui` (a GUI app started hidden can get its
   * first window hidden too). `cwd` defaults to this process's cwd.
   */
  spawnDetached(argv: string[], opts?: { gui?: boolean; cwd?: string }): void;
  /** Removes these paths once this process has exited (a running exe cannot delete itself). */
  removeAfterExit(paths: string[]): void;
  isAlive(pid: number): boolean;
  killTree(pid: number): Promise<void>;
  runDaemonForeground(): Promise<ForegroundResult>;
  runTui(opts: { warning?: string }): Promise<void>;
  /** Runs the bridge on stdin/stdout until stdin closes; returns the exit code. */
  runBridge(link: BridgeLink): Promise<number>;
  sleep(ms: number): Promise<void>;
}

const WAIT_MS = 5_000;
const POLL_MS = 100;
/** How long a locked exe may keep a rename failing (the tray's bridge hub.exe may still be exiting). */
const RENAME_WAIT_MS = 10_000;

class UsageError extends Error {}

function samePath(a: string, b: string): boolean {
  const n = (p: string) => resolve(p).toLowerCase();
  return n(a) === n(b);
}

function logPath(d: CliDeps): string {
  return join(d.paths.runDir, "daemon.log");
}

function readPid(d: CliDeps, file: string): number | null {
  const text = d.fs.readText(file)?.trim() ?? "";
  if (!/^\d+$/.test(text)) return null;
  const pid = Number(text);
  return Number.isSafeInteger(pid) && pid > 0 ? pid : null;
}

/** daemon.lock: `<pid> <startedAtMs>`, or `<pid>` from an older version; null when missing or garbage. */
function readLock(d: CliDeps): LockInfo | null {
  const text = d.fs.readText(join(d.paths.runDir, LOCK_FILE));
  return text === null ? null : parseLock(text);
}

/**
 * What the lock's process is (see DaemonIdentity): by pid and start time, or by image for a legacy
 * lock. After an unclean logoff daemon.lock can name a pid that Windows gave to another process since.
 */
function identify(d: CliDeps, lock: LockInfo): DaemonIdentity {
  return identifyDaemon(
    lock,
    {
      isAlive: d.isAlive,
      startTime: (p) => d.platform.processStartTime(p),
      imagePath: (p) => d.platform.processImagePath(p),
    },
    !d.compiled,
  );
}

/**
 * The lock held by our daemon, or by a live process that cannot be verified (never start a second
 * daemon beside it). Null for no lock, a dead pid, or a pid reused by another process.
 */
function heldLock(d: CliDeps): LockInfo | null {
  const lock = readLock(d);
  return lock !== null && identify(d, lock) !== "foreign" ? lock : null;
}

function runningPid(d: CliDeps): number | null {
  return heldLock(d)?.pid ?? null;
}

async function waitFor<T>(
  d: CliDeps,
  probe: () => T | null | Promise<T | null>,
): Promise<T | null> {
  for (let waited = 0; ; waited += POLL_MS) {
    const v = await probe();
    if (v !== null) return v;
    if (waited >= WAIT_MS) return null;
    await d.sleep(POLL_MS);
  }
}

type StartResult = { kind: "already" | "started"; pid: number } | { kind: "failed" };

async function startDaemon(d: CliDeps, daemonArgv: string[]): Promise<StartResult> {
  const pid = runningPid(d);
  d.fs.remove(join(d.paths.runDir, DAEMON_STOPPED_FILE));
  if (pid !== null) return { kind: "already", pid };
  // cwd is the exe's own folder (bin when installed), so the daemon never pins the caller's folder.
  d.spawnDetached(daemonArgv, { cwd: dirname(daemonArgv[0] ?? ".") });
  const started = await waitFor(d, () => runningPid(d));
  return started === null ? { kind: "failed" } : { kind: "started", pid: started };
}

/**
 * True when stopped (or never running). Only our daemon is killed: its identity ("ours") and its image
 * (hubd.exe, hub.exe or bun.exe) are both checked right before the kill. A lock naming another
 * process is stale and never acted on; one that cannot be verified is left running (false).
 */
async function stopDaemon(d: CliDeps): Promise<boolean> {
  const lock = heldLock(d);
  if (lock === null) return true;
  const { pid } = lock;
  if (identify(d, lock) !== "ours" || !isKillableImage(d.platform.processImagePath(pid))) {
    d.err(
      `daemon: pid=${pid} holds daemon.lock but cannot be verified as the daemon (it may run ` +
        "elevated); not stopped. Stop it by hand.",
    );
    return false;
  }
  await d.killTree(pid);
  const gone = await waitFor(d, () => (identify(d, lock) === "foreign" ? true : null));
  if (gone === null) return false;
  // A killed daemon cannot release its lock; drop it if it still names that pid.
  if (readLock(d)?.pid === pid) d.fs.remove(join(d.paths.runDir, LOCK_FILE));
  return true;
}

/** Drops tray.pid when it still names `pid` (a killed or vanished tray cannot delete it). */
function dropTrayPid(d: CliDeps, pid: number): void {
  if (readPid(d, d.paths.trayPidFile) === pid) d.fs.remove(d.paths.trayPidFile);
}

/** True when `pid` is alive and runs `<bin>\SessionHubTray.exe`. */
function isOurTray(d: CliDeps, pid: number): boolean {
  if (!d.isAlive(pid)) return false;
  const image = d.platform.processImagePath(pid);
  return image !== null && samePath(image, d.paths.installedTrayExe);
}

/** The pid in tray.pid when that process is alive and runs `<bin>\SessionHubTray.exe`; else null. */
function ourTrayPid(d: CliDeps): number | null {
  const pid = readPid(d, d.paths.trayPidFile);
  return pid !== null && isOurTray(d, pid) ? pid : null;
}

/**
 * Stops the tray; null when no tray runs afterwards, else why one still does.
 * - A tray.pid naming a live process that is not `<bin>\SessionHubTray.exe` (the pid was reused after
 *   the tray died) is never killed; the file is dropped.
 * - The quit event is set even without a usable pid (harmless when no tray runs). The tray saves its
 *   drafts and exits; this waits up to 5 s for its instance mutex and its pid to go away.
 * - Only a pid whose image was verified is killed, and only after that wait.
 */
async function stopTray(d: CliDeps): Promise<string | null> {
  const recorded = readPid(d, d.paths.trayPidFile);
  const pid = ourTrayPid(d);
  if (pid === null && recorded !== null && d.isAlive(recorded)) {
    d.out(
      `tray: tray.pid points at another program (pid=${recorded}), or at a tray running elevated; ` +
        "left alone",
    );
    dropTrayPid(d, recorded);
  }
  const signalled = await d.platform.signalTrayQuit();
  if (!signalled && pid === null && !(await d.platform.trayInstanceExists())) return null;
  d.out(pid === null ? "tray: asking it to quit" : `tray: asking pid=${pid} to quit`);
  const gone = async (): Promise<true | null> =>
    (pid !== null && d.isAlive(pid)) || (await d.platform.trayInstanceExists()) ? null : true;
  if ((await waitFor(d, gone)) === null) {
    // Checked again right before the kill: during the wait the tray may have exited and its pid
    // been reused by another program.
    if (pid !== null && isOurTray(d, pid)) {
      await d.killTree(pid);
      if ((await waitFor(d, () => (d.isAlive(pid) ? null : true))) === null) {
        return `tray pid=${pid} did not stop`;
      }
    } else if (await d.platform.trayInstanceExists()) {
      return "a tray without a verified tray.pid did not quit (it may run elevated); quit it from its menu";
    }
  }
  if (pid !== null) dropTrayPid(d, pid);
  d.out("tray: STOPPED");
  return null;
}

async function startTray(d: CliDeps): Promise<StartResult> {
  const pid = ourTrayPid(d);
  if (pid !== null) return { kind: "already", pid };
  d.spawnDetached([d.paths.installedTrayExe], { gui: true, cwd: d.paths.binDir });
  const started = await waitFor(d, () => ourTrayPid(d));
  return started === null ? { kind: "failed" } : { kind: "started", pid: started };
}

/** Prints the tray start result; false when it failed. */
function reportTray(t: StartResult, d: CliDeps): boolean {
  if (t.kind === "failed") {
    d.err(
      `tray: FAILED: ${TRAY_EXE} did not start within 5 s; see ${join(d.paths.runDir, "tray.log")}`,
    );
    return false;
  }
  d.out(`tray: ${t.kind === "already" ? "ALREADY RUNNING" : "STARTED"} pid=${t.pid}`);
  return true;
}

/** Renames, retrying for up to 10 s while the target is locked. Throws the last error. */
async function replaceFile(d: CliDeps, tmp: string, dst: string): Promise<void> {
  for (let waited = 0; ; waited += POLL_MS) {
    try {
      d.fs.rename(tmp, dst);
      return;
    } catch (e) {
      if (waited >= RENAME_WAIT_MS) throw e;
    }
    await d.sleep(POLL_MS);
  }
}

/**
 * After a failed swap: start whatever is installed again, so the daemon and the tray are not left
 * stopped until the next login. Returns the exit code 1.
 */
async function restartInstalled(d: CliDeps): Promise<number> {
  const p = d.paths;
  if (d.fs.exists(p.installedDaemonExe)) {
    const r = await startDaemon(d, [p.installedDaemonExe]);
    if (r.kind === "failed") d.err(`daemon: ${startLine(r, d)}`);
    else d.out(`daemon: ${startLine(r, d)} (the installed version)`);
  }
  if (d.fs.exists(p.installedTrayExe)) reportTray(await startTray(d), d);
  return 1;
}

function startLine(r: StartResult, d: CliDeps): string {
  if (r.kind === "already") return `ALREADY RUNNING pid=${r.pid}`;
  if (r.kind === "started") return `STARTED pid=${r.pid}`;
  return `FAILED: the daemon did not start within 5 s; see ${logPath(d)}`;
}

async function cmdDaemon(sub: string, d: CliDeps): Promise<number> {
  switch (sub) {
    case "status": {
      const pid = runningPid(d);
      d.out(pid === null ? "STOPPED" : `RUNNING pid=${pid}`);
      return pid === null ? 3 : 0;
    }
    case "start": {
      const r = await startDaemon(d, d.daemonArgv);
      if (r.kind === "failed") {
        d.err(startLine(r, d));
        return 1;
      }
      d.out(startLine(r, d));
      return 0;
    }
    case "stop": {
      const pid = runningPid(d);
      markStopped(d);
      if (await stopDaemon(d)) {
        d.out("STOPPED");
        return 0;
      }
      d.err(`FAILED: pid=${pid} is still running`);
      return 1;
    }
    case "run": {
      const r = await d.runDaemonForeground();
      if (r === "already-running") d.out("ALREADY RUNNING");
      return 0;
    }
    default:
      throw new UsageError();
  }
}

async function cmdTui(d: CliDeps): Promise<number> {
  if (!d.fs.exists(d.paths.dataDir)) await d.platform.ensurePrivateDir(d.paths.dataDir);
  const r = await startDaemon(d, d.daemonArgv);
  const opts: { warning?: string } = {};
  if (r.kind === "failed") {
    opts.warning = `The daemon is not running and did not start; the list may be stale. See ${logPath(d)}`;
  }
  await d.runTui(opts);
  return 0;
}

/** stdout is the bridge's data channel: nothing here may print to it. */
async function cmdBridge(d: CliDeps): Promise<number> {
  return d.runBridge({
    daemonRunning: () => runningPid(d) !== null,
    stopRequested: () => d.fs.exists(join(d.paths.runDir, DAEMON_STOPPED_FILE)),
    startDaemon: async () => {
      if (!d.fs.exists(d.paths.dataDir)) await d.platform.ensurePrivateDir(d.paths.dataDir);
      return (await startDaemon(d, d.daemonArgv)).kind !== "failed";
    },
  });
}

/** Paths in the shim folder that would shadow or clash with ours. Empty when install may proceed. */
function foreignShims(d: CliDeps): string[] {
  const { shimDir, shimPath } = d.paths;
  const found: string[] = [];
  const cmd = d.fs.readText(shimPath);
  if (cmd !== null && cmd !== SHIM_CONTENT) found.push(shimPath);
  else if (cmd === null && d.fs.exists(shimPath)) found.push(shimPath);
  const exe = join(shimDir, "hub.exe");
  if (d.fs.exists(exe)) found.push(exe);
  return found;
}

async function cmdInstall(d: CliDeps, allowUnreviewed: boolean): Promise<number> {
  const p = d.paths;
  if (!d.compiled) {
    d.err("hub install: run from dist\\hub.exe (build it with `bun run build`), not from source.");
    return 1;
  }
  const daemonSource = join(dirname(d.exePath), DAEMON_EXE);
  if (!d.fs.exists(daemonSource)) {
    d.err(`hub install: ${daemonSource} is missing; build the exes with \`bun run build\`.`);
    return 1;
  }
  const traySource = join(dirname(d.exePath), TRAY_EXE);
  if (!d.fs.exists(traySource)) {
    d.err(`hub install: ${traySource} is missing; build it with \`bun run build\`.`);
    return 1;
  }
  let manifest: Manifest | null = null;
  if (!samePath(d.exePath, p.installedExe)) {
    manifest = checkBuild(d, dirname(d.exePath));
    if (manifest === null) return 1;
    if (manifest.dirty && !allowUnreviewed) {
      d.err(
        "hub install: refusing: these exes were built from uncommitted changes. Commit and rebuild, or pass --allow-unreviewed.",
      );
      return 1;
    }
  }
  const foreign = foreignShims(d);
  if (foreign.length > 0) {
    for (const f of foreign) {
      d.err(`hub install: refusing: ${f} exists and was not created by Session Hub.`);
    }
    d.err("Rename or remove it, then run hub install again.");
    return 1;
  }

  await d.platform.ensurePrivateDir(p.dataDir);
  d.fs.mkdirp(p.runDir);
  d.fs.mkdirp(p.relayDir);
  d.out(`data dir: ${p.dataDir} (owner-only)`);

  // Stop the tray first in every case: its bridge runs hub.exe, its exe may be replaced, and it is
  // started again below either way.
  const trayStop = await stopTray(d);
  if (trayStop !== null) {
    d.err(`hub install: FAILED: ${trayStop}`);
    return 1;
  }

  // From here the tray (and maybe the daemon) is stopped: no failure may leave them down until login.
  try {
    return await installStopped(d, daemonSource, traySource, manifest);
  } catch (e) {
    d.err(`hub install: FAILED: ${e instanceof Error ? e.message : String(e)}`);
    return restartInstalled(d);
  }
}

/**
 * The build manifest in `distDir`, when it is complete and every exe in `distDir` matches its hash
 * (INF-08c). Otherwise null, with the reason on stderr: nothing has been stopped or copied yet.
 */
function checkBuild(d: CliDeps, distDir: string): Manifest | null {
  const text = d.fs.readText(join(distDir, MANIFEST_FILE));
  const manifest = text === null ? null : parseManifest(text);
  if (manifest === null) {
    d.err(
      `hub install: ${join(distDir, MANIFEST_FILE)} is missing or incomplete; build the exes with \`bun run build\`.`,
    );
    return null;
  }
  for (const [name, want] of Object.entries(manifest.sha256)) {
    const file = join(distDir, name);
    let got: string;
    try {
      got = d.fs.sha256(file);
    } catch (e) {
      d.err(`hub install: cannot read ${file}: ${(e as Error).message}`);
      return null;
    }
    if (got !== want) {
      d.err(
        `hub install: refusing: ${file} does not match the build manifest; rebuild with \`bun run build\`.`,
      );
      return null;
    }
  }
  d.out(`build: ${describeBuild(manifest)}`);
  return manifest;
}

/** Removes each path, ignoring failures: a leftover .new or .old is harmless. */
function removeQuietly(d: CliDeps, paths: string[]): void {
  for (const path of paths) {
    try {
      d.fs.remove(path);
    } catch {
      // the original error, if any, is the one to report
    }
  }
}

/**
 * Replaces the installed exes as one unit, so a failure never leaves a mix of old and new ones:
 * 1. copy every source to <dst>.new and check it against the build manifest;
 * 2. copy every installed exe to <dst>.old;
 * 3. drop the old bin manifest, then swap each .new in (locked files are retried for 10 s);
 * 4. on a failed swap, put the .old copies back over the exes already swapped.
 * False after a failure, with the reason on stderr; the caller then restarts what is installed.
 */
async function swapAll(
  d: CliDeps,
  copies: [string, string][],
  manifest: Manifest | null,
): Promise<boolean> {
  const p = d.paths;
  const staged: string[] = [];
  const backups: [string, string][] = [];
  const cleanup = () => removeQuietly(d, [...staged, ...backups.map(([old]) => old)]);

  for (const [src, dst] of copies) {
    const tmp = `${dst}.new`;
    staged.push(tmp);
    try {
      d.fs.copy(src, tmp);
      if (manifest !== null) {
        const want = manifest.sha256[basename(dst)];
        if (want === undefined) throw new Error("the build manifest has no hash for it");
        if (d.fs.sha256(tmp) !== want)
          throw new Error("the copy does not match the build manifest");
      }
    } catch (e) {
      cleanup();
      d.err(`hub install: cannot copy ${src} to ${tmp}: ${(e as Error).message}`);
      return false;
    }
  }

  for (const [, dst] of copies) {
    if (!d.fs.exists(dst)) continue;
    const old = `${dst}.old`;
    try {
      d.fs.copy(dst, old);
      backups.push([old, dst]);
    } catch (e) {
      removeQuietly(d, [old]);
      cleanup();
      d.err(`hub install: cannot back up ${dst}: ${(e as Error).message}`);
      return false;
    }
  }

  // The old manifest describes the old exes; a new one is written only after the last swap. Its
  // text is kept so a rollback can put it back with the old exes.
  const binManifest = join(p.binDir, MANIFEST_FILE);
  const oldManifest = d.fs.readText(binManifest);
  removeQuietly(d, [binManifest]);
  const done: string[] = [];
  for (const [, dst] of copies) {
    try {
      await replaceFile(d, `${dst}.new`, dst);
    } catch (e) {
      d.err(`hub install: cannot replace ${dst}: ${(e as Error).message}`);
      d.err(
        "Close any open hub window and quit the Session Hub tray app, then run hub install again.",
      );
      // A backup that could not be put back is the only copy of that old exe: it stays.
      const kept: string[] = [];
      for (const swappedDst of done) {
        const backup = backups.find(([, b]) => b === swappedDst);
        if (backup === undefined) continue;
        try {
          await replaceFile(d, backup[0], swappedDst);
        } catch (re) {
          kept.push(backup[0]);
          d.err(`hub install: could not put back ${swappedDst}: ${(re as Error).message}`);
          d.err(`hub install: the old version is kept at ${backup[0]}`);
        }
      }
      removeQuietly(d, [
        ...staged,
        ...backups.map(([old]) => old).filter((old) => !kept.includes(old)),
      ]);
      if (kept.length === 0 && oldManifest !== null) {
        try {
          d.fs.writeText(binManifest, oldManifest);
        } catch {
          // the old exes are back; a missing manifest only loses the trace
        }
      }
      return false;
    }
    done.push(dst);
    d.out(`binary: ${dst}`);
  }
  cleanup();
  return true;
}

/** The install steps after the tray was stopped. A throw is rolled back by the caller. */
async function installStopped(
  d: CliDeps,
  daemonSource: string,
  traySource: string,
  manifest: Manifest | null,
): Promise<number> {
  const p = d.paths;
  if (samePath(d.exePath, p.installedExe)) {
    d.out(`binary: already running from ${p.installedExe}`);
  } else {
    const pid = runningPid(d);
    if (pid !== null) {
      d.out(`daemon: stopping pid=${pid} to replace the binary`);
      markStopped(d); // the start after the copy clears it
      if (!(await stopDaemon(d))) {
        d.err(`hub install: FAILED: daemon pid=${pid} did not stop`);
        return restartInstalled(d);
      }
    }
    d.fs.mkdirp(p.binDir);
    const copies: [string, string][] = [
      [d.exePath, p.installedExe],
      [daemonSource, p.installedDaemonExe],
      [traySource, p.installedTrayExe],
    ];
    const swapped = await swapAll(d, copies, manifest);
    if (!swapped) return restartInstalled(d);
    // Kept beside the exes so the installed build can be traced to its commit and hashes.
    if (manifest !== null) d.fs.writeText(join(p.binDir, MANIFEST_FILE), formatManifest(manifest));
  }

  d.fs.mkdirp(p.shimDir);
  if (d.fs.readText(p.shimPath) !== SHIM_CONTENT) d.fs.writeText(p.shimPath, SHIM_CONTENT);
  d.out(`command: ${p.shimPath}`);

  await d.platform.setAutostart("SessionHubDaemon", p.installedDaemonExe);
  d.out(`autostart: ${p.installedDaemonExe} (at login)`);
  await d.platform.setAutostart("SessionHubTray", p.installedTrayExe);
  d.out(`autostart: ${p.installedTrayExe} (at login)`);

  const r = await startDaemon(d, [p.installedDaemonExe]);
  if (r.kind === "failed") {
    d.err(`daemon: ${startLine(r, d)}`);
    reportTray(await startTray(d), d); // the tray still comes back; it shows the daemon as stopped
    return 1;
  }
  d.out(`daemon: ${startLine(r, d)}`);

  if (!reportTray(await startTray(d), d)) return 1;
  d.out(`Installed. Run "hub" in a new terminal. Data: ${p.dataDir}`);
  return 0;
}

async function cmdUninstall(purge: boolean, d: CliDeps): Promise<number> {
  const p = d.paths;
  let code = 0;
  const trayStop = await stopTray(d);
  if (trayStop !== null) {
    d.err(`tray: FAILED: ${trayStop}; remove the rest by hand after it exits`);
    code = 1;
  }
  await d.platform.clearAutostart("SessionHubDaemon");
  await d.platform.clearAutostart("SessionHubTray");
  d.out("autostart: cleared");

  markStopped(d);
  if (await stopDaemon(d)) d.out("daemon: STOPPED");
  else {
    d.err("daemon: FAILED to stop; remove the rest by hand after it exits");
    code = 1;
  }

  const shim = d.fs.readText(p.shimPath);
  if (shim === SHIM_CONTENT) {
    d.fs.remove(p.shimPath);
    d.out(`command: removed ${p.shimPath}`);
  } else if (shim !== null || d.fs.exists(p.shimPath)) {
    d.out(`command: ${p.shimPath} was not created by Session Hub; left in place`);
  }

  // A running exe cannot delete itself on Windows: hand its folder to a helper that waits for us.
  const selfInBin = samePath(dirname(d.exePath), p.binDir);
  const target = purge ? p.dataDir : p.binDir;
  const removeNow = (path: string) => {
    try {
      d.fs.remove(path);
      return true;
    } catch (e) {
      d.err(`cannot remove ${path}: ${(e as Error).message}`);
      code = 1;
      return false;
    }
  };

  if (selfInBin) {
    if (purge) {
      // Remove everything except bin now (the db must go even if the helper fails).
      for (const sub of [p.dbPath, `${p.dbPath}-wal`, `${p.dbPath}-shm`, p.runDir, p.relayDir]) {
        removeNow(sub);
      }
    }
    d.removeAfterExit([target]);
    d.out(`removing ${target} after this command exits`);
  } else {
    try {
      d.fs.remove(target);
      d.out(`removed ${target}`);
    } catch {
      // A just-killed hubd.exe can stay locked for a moment.
      d.removeAfterExit([target]);
      d.out(`removing ${target} after this command exits`);
    }
  }
  if (!purge) d.out(`data: kept ${p.dataDir} (run "hub uninstall" again to delete it)`);
  return code;
}

async function dispatch(argv: string[], d: CliDeps): Promise<number> {
  const [cmd, ...rest] = argv;
  if (cmd === undefined) return cmdTui(d);
  const noArgs = () => {
    if (rest.length > 0) throw new UsageError();
  };
  switch (cmd) {
    case "--version":
    case "-v":
      noArgs();
      d.out(d.version);
      return 0;
    case "--help":
    case "-h":
    case "help":
      noArgs();
      d.out(USAGE);
      return 0;
    case "daemon": {
      const [sub, ...more] = rest;
      if (sub === undefined || more.length > 0) throw new UsageError();
      return cmdDaemon(sub, d);
    }
    case "install": {
      if (rest.length > 1 || (rest.length === 1 && rest[0] !== "--allow-unreviewed")) {
        throw new UsageError();
      }
      return cmdInstall(d, rest[0] === "--allow-unreviewed");
    }
    case "bridge":
      noArgs();
      return cmdBridge(d);
    case "uninstall": {
      // The data goes by default (SH42): nothing expires it once the daemon is gone. `--purge` is the
      // old spelling of the default and still accepted.
      const flag = rest[0];
      if (rest.length > 1 || (flag !== undefined && flag !== "--keep-data" && flag !== "--purge")) {
        throw new UsageError();
      }
      return cmdUninstall(flag !== "--keep-data", d);
    }
    default:
      throw new UsageError();
  }
}

/**
 * hubd.exe argv: none means `daemon run`; `daemon ...` and `--version` pass through. Anything else
 * (install, the TUI) belongs to hub.exe: null.
 */
export function hubdArgv(argv: string[]): string[] | null {
  if (argv.length === 0) return ["daemon", "run"];
  if (argv[0] === "daemon" || argv[0] === "--version") return argv;
  return null;
}

/** Runs one CLI command. Returns the process exit code: 0 ok, 1 failure, 2 usage, 3 stopped. */
export async function runCli(argv: string[], deps: CliDeps): Promise<number> {
  try {
    return await dispatch(argv, deps);
  } catch (e) {
    if (e instanceof UsageError) {
      deps.err(USAGE);
      return 2;
    }
    deps.err(`hub: ${e instanceof Error ? e.message : String(e)}`);
    return 1;
  }
}
