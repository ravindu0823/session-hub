import { spawn } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { basename, dirname, join } from "node:path";
import pkg from "../../package.json" with { type: "json" };
import { readLines } from "../bridge/protocol.ts";
import { type BridgeDeps, runBridge } from "../bridge/server.ts";
import { DATA_DIR, PROFILES, RELAY_LOG } from "../config.ts";
import { identifyDaemon, type LockInfo } from "../daemon/lock.ts";
import { createLogger } from "../daemon/log.ts";
import { realClaudeCli } from "../discovery/agents.ts";
import { runMailCli } from "../mail/cli.ts";
import { type RolesConfig, rolesLoader } from "../mail/roles.ts";
import { mailUpkeep } from "../mail/service.ts";
import { createWindowsPlatform } from "../platform/windows/index.ts";
import { sysbin } from "../platform/windows/sysbin.ts";
import { claimMarkerFile, runGuard } from "../send/guard.ts";
import { send } from "../send/relay.ts";
import { deliveryChecker, expireSends } from "../send/verify.ts";
import { Store } from "../store/db.ts";
import {
  type BridgeLink,
  type CliDeps,
  type CliPaths,
  cliPaths,
  DAEMON_EXE,
  nodeFs,
} from "./commands.ts";
import { daemonRun } from "./daemon-run.ts";

// Real side effects for runCli. Shared by hub.exe (src/cli.ts) and the console-less hubd.exe
// (src/daemon-main.ts), so it must never import Ink or the TUI.

const KILL_BOUND_MS = 5_000;

function isAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (e) {
    return (e as NodeJS.ErrnoException).code === "EPERM";
  }
}

/**
 * Detached (survives our exit: Bun kills attached children when it exits), no console, hidden,
 * stdio ignored. A process started this way has no console, which Bun and cmd.exe tolerate but
 * powershell.exe does not. `hide: false` is for a GUI app: Windows can apply the hidden show state to
 * a process's first window, so a tray started hidden would not show its first panel.
 */
function spawnDetached(
  argv: string[],
  opts: { env?: Record<string, string>; verbatim?: boolean; hide?: boolean; cwd?: string } = {},
): void {
  const [cmd, ...args] = argv;
  if (cmd === undefined) throw new Error("spawnDetached: empty argv");
  const child = spawn(cmd, args, {
    detached: true,
    stdio: "ignore",
    windowsHide: opts.hide ?? true,
    windowsVerbatimArguments: opts.verbatim ?? false,
    env: opts.env ? { ...process.env, ...opts.env } : process.env,
    ...(opts.cwd === undefined ? {} : { cwd: opts.cwd }),
  });
  child.on("error", () => {}); // a failed spawn shows up as "did not start"
  child.unref();
}

/** taskkill the whole tree, bounded; never throws. */
async function killTree(pid: number): Promise<void> {
  try {
    const tk = Bun.spawn([sysbin.taskkill(), "/PID", String(pid), "/T", "/F"], {
      stdin: "ignore",
      stdout: "ignore",
      stderr: "ignore",
      windowsHide: true,
    });
    let timer: ReturnType<typeof setTimeout> | undefined;
    await Promise.race([
      tk.exited,
      new Promise<void>((r) => {
        timer = setTimeout(r, KILL_BOUND_MS);
      }),
    ]);
    clearTimeout(timer);
  } catch {
    // the caller checks liveness afterwards
  }
}

// Retries for about 30 s until the folder is gone (a running exe is locked until it exits). The path
// goes in by environment variable, so it is never parsed as part of the command line.
const RM_LOOP =
  'for /l %i in (1,1,30) do @(rmdir /s /q "%HUB_RM%" 2>nul & ' +
  'if not exist "%HUB_RM%" exit /b 0 & ping -n 2 127.0.0.1 >nul)';

/** One hidden cmd.exe per path deletes it once this process has exited. */
function removeAfterExit(paths: string[]): void {
  for (const p of paths) {
    spawnDetached([sysbin.cmd(), "/d", "/c", RM_LOOP], { env: { HUB_RM: p }, verbatim: true });
  }
}

/** The local roles file (never in the repository): null when there is none. */
function readRolesFile(dataDir: string): string | null {
  try {
    return readFileSync(join(dataDir, "roles.json"), "utf8");
  } catch (e) {
    if ((e as NodeJS.ErrnoException).code === "ENOENT") return null;
    throw e;
  }
}

/** Pure: how the relay's hook runs `hub relay-guard` from this executable. */
export function relayGuardArgvFor(opts: {
  execPath: string;
  compiled: boolean;
  entryPath: string;
}): string[] {
  return opts.compiled
    ? [opts.execPath, "relay-guard"]
    : [opts.execPath, opts.entryPath, "relay-guard"];
}

/** Pure: how to start the daemon from this executable. */
export function daemonArgvFor(opts: {
  execPath: string;
  compiled: boolean;
  entryPath: string;
  exists: (path: string) => boolean;
}): string[] {
  if (!opts.compiled) return [opts.execPath, opts.entryPath, "daemon", "run"];
  const hubd = join(dirname(opts.execPath), DAEMON_EXE);
  return opts.exists(hubd) ? [hubd] : [opts.execPath, "daemon", "run"];
}

/**
 * `hub bridge` with real side effects: protocol lines on stdout, requests from stdin, the index
 * opened only once hub.db exists (the bridge never creates it), sends through the real relay.
 */
export async function runRealBridge(
  link: BridgeLink,
  opts: { paths: CliPaths; version: string; guardArgv: readonly string[] },
): Promise<number> {
  const { paths } = opts;
  let store: Store | null = null;
  const deps: BridgeDeps = {
    store: () => {
      // create:false: a hub.db deleted between the check and the open is never re-created.
      if (store === null && existsSync(paths.dbPath)) {
        store = Store.open(paths.dbPath, { create: false });
      }
      return store;
    },
    daemonRunning: link.daemonRunning,
    daemonStopRequested: link.stopRequested,
    startDaemon: link.startDaemon,
    send: (req, onCreated) => {
      const s = deps.store();
      if (s === null) throw new Error("the index does not exist yet");
      return send(req, {
        store: s,
        cli: realClaudeCli(),
        profiles: PROFILES,
        relayDir: paths.relayDir,
        now: () => Date.now(),
        onCreated,
        log: createLogger(paths.runDir, undefined, RELAY_LOG),
        guardArgv: opts.guardArgv,
      });
    },
    now: () => Date.now(),
    sleep: (ms) => Bun.sleep(ms),
    version: opts.version,
  };
  // A broken stdout means the tray cannot hear us: say so once on stderr and stop with exit 1.
  let broken = false;
  let stop: (code: number) => void = () => {};
  const stopped = new Promise<number>((r) => {
    stop = r;
  });
  const fail = (e: unknown): void => {
    if (broken) return;
    broken = true;
    const why = e instanceof Error ? e.message : String(e);
    process.stderr.write(`hub bridge: stdout failed (${why}); stopping\n`);
    stop(1);
  };
  process.stdout.on("error", fail);
  try {
    return await runBridge(
      {
        lines: readLines(Bun.stdin.stream()),
        write: (line) => {
          if (broken) return;
          try {
            process.stdout.write(`${line}\n`);
          } catch (e) {
            fail(e);
          }
        },
        stop: stopped,
      },
      deps,
    );
  } finally {
    if (!broken) await new Promise<void>((r) => process.stdout.write("", () => r()));
    (store as Store | null)?.close();
  }
}

/** `entryPath` is the running script in dev (import.meta.path of the entry). */
export function realCliDeps(opts: {
  entryPath: string;
  runTui: CliDeps["runTui"];
  runBridge: CliDeps["runBridge"];
}): CliDeps {
  const exe = basename(process.execPath).toLowerCase();
  const compiled = /[\\/]~BUN[\\/]/.test(Bun.main) || exe === "hub.exe" || exe === DAEMON_EXE;
  const platform = createWindowsPlatform();
  const paths = cliPaths({
    dataDir: DATA_DIR,
    userProfile: process.env.USERPROFILE ?? homedir(),
  });
  const now = () => Date.now();
  let loadRoles: (() => RolesConfig) | null = null;
  const daemonIdentity = (lock: LockInfo) =>
    identifyDaemon(
      lock,
      {
        isAlive,
        startTime: (p) => platform.processStartTime(p),
        imagePath: (p) => platform.processImagePath(p),
      },
      !compiled,
    );
  return {
    platform,
    paths,
    version: pkg.version,
    exePath: process.execPath,
    compiled,
    daemonArgv: daemonArgvFor({
      execPath: process.execPath,
      compiled,
      entryPath: opts.entryPath,
      exists: existsSync,
    }),
    fs: nodeFs,
    out: (line) => process.stdout.write(`${line}\n`),
    err: (line) => process.stderr.write(`${line}\n`),
    spawnDetached: (argv, o) =>
      spawnDetached(argv, { hide: !o?.gui, ...(o?.cwd === undefined ? {} : { cwd: o.cwd }) }),
    removeAfterExit,
    isAlive,
    killTree,
    runDaemonForeground: () =>
      daemonRun({
        platform,
        paths,
        profiles: PROFILES,
        cli: realClaudeCli(),
        now,
        daemonIdentity,
        onRecords: (store, now, log) =>
          deliveryChecker(store, now, (s) =>
            log.warn("relay_stray", { sessionId: s.sessionId, ts: s.ts }),
          ),
        expireSends,
        mailUpkeep: (store, now, log) => {
          loadRoles ??= rolesLoader(
            () => readRolesFile(paths.dataDir),
            (event) => log.warn(event),
          );
          return mailUpkeep(store, loadRoles(), now());
        },
      }),
    runTui: opts.runTui,
    runBridge: opts.runBridge,
    guardArgv: relayGuardArgvFor({
      execPath: process.execPath,
      compiled,
      entryPath: opts.entryPath,
    }),
    runRelayGuard: async () =>
      runGuard(await Bun.stdin.text(), {
        env: process.env,
        claimMarker: claimMarkerFile,
        err: (line) =>
          process.stderr.write(`${line}
`),
      }),
    runMail: (cmd, args) =>
      runMailCli(cmd, args, {
        // The CLI never creates hub.db and never upgrades its schema: the daemon does both.
        openStore: () =>
          existsSync(paths.dbPath)
            ? Store.open(paths.dbPath, { create: false, migrate: false })
            : null,
        readRoles: () => readRolesFile(paths.dataDir),
        env: process.env,
        now,
        out: (line) => process.stdout.write(`${line}\n`),
        err: (line) => process.stderr.write(`${line}\n`),
        readInput: async (p) => (p === "-" ? await Bun.stdin.text() : readFileSync(p, "utf8")),
        sleep: (ms) => Bun.sleep(ms),
      }),
    sleep: (ms) => Bun.sleep(ms),
  };
}
