import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  type BridgeLink,
  type CliDeps,
  cliPaths,
  DAEMON_STOPPED_FILE,
  hubdArgv,
  nodeFs,
  runCli,
  SHIM_CONTENT,
  USAGE,
} from "../src/cli/commands.ts";
import { daemonRun } from "../src/cli/daemon-run.ts";
import { daemonArgvFor } from "../src/cli/real-deps.ts";
import type { DaemonDeps } from "../src/daemon/collector.ts";
import { identifyDaemon, LOCK_FILE } from "../src/daemon/lock.ts";
import { FakePlatform } from "../src/platform/fake.ts";
import { writeBuildManifest } from "./fakes/build-manifest.ts";

const DAEMON_PID = 4242;
const TRAY_PID = 5151;
const OTHER_IMAGE = "C:\\Windows\\System32\\notepad.exe";

let root: string;
let platform: FakePlatform;
let out: string[];
let err: string[];
let alive: Set<number>;
let spawned: string[][];
let killed: number[];
let removedAfterExit: string[][];
let tuiRuns: { warning?: string }[];
let bridgeLinks: BridgeLink[];
let bridgeExit: number;
let foregroundRuns: number;
let spawnStartsDaemon: boolean;
/** Start time of a live pid; a live pid missing here cannot be queried (null). */
let starts: Map<number, number>;
const T0 = 1_763_000_000_000;

/**
 * Image of a live pid other than the tray and hubd.exe: another program (a reused pid), hub.exe or
 * bun.exe, or null (alive but unreadable, e.g. elevated).
 */
let foreign: Map<number, string | null>;

function paths() {
  return cliPaths({ dataDir: join(root, "data"), userProfile: join(root, "home") });
}

function lockPath(): string {
  return join(paths().runDir, LOCK_FILE);
}

/** `startedAt` writes the current `<pid> <startMs>` form; without it the legacy pid-only form. */
function writeLock(pid: number, startedAt?: number): void {
  mkdirSync(paths().runDir, { recursive: true });
  writeFileSync(lockPath(), startedAt === undefined ? String(pid) : `${pid} ${startedAt}`);
}

function makeDeps(over: Partial<CliDeps> = {}): CliDeps {
  return {
    platform,
    paths: paths(),
    version: "9.8.7",
    exePath: join(root, "dist", "hub.exe"),
    compiled: true,
    daemonArgv: [join(root, "dist", "hubd.exe")],
    fs: nodeFs,
    out: (l) => out.push(l),
    err: (l) => err.push(l),
    spawnDetached: (argv) => {
      spawned.push(argv);
      if (argv[0] === paths().installedTrayExe) {
        mkdirSync(paths().runDir, { recursive: true });
        writeFileSync(paths().trayPidFile, String(TRAY_PID));
        alive.add(TRAY_PID);
      } else if (spawnStartsDaemon) {
        writeLock(DAEMON_PID);
        alive.add(DAEMON_PID);
      }
    },
    removeAfterExit: (p) => {
      removedAfterExit.push(p);
    },
    isAlive: (pid) => alive.has(pid),
    killTree: async (pid) => {
      killed.push(pid);
      alive.delete(pid);
    },
    runDaemonForeground: async () => {
      foregroundRuns += 1;
      return "stopped";
    },
    runTui: async (opts) => {
      tuiRuns.push(opts);
    },
    runBridge: async (link) => {
      bridgeLinks.push(link);
      return bridgeExit;
    },
    sleep: async () => {},
    ...over,
  };
}

beforeEach(() => {
  root = mkdtempSync(join(tmpdir(), "hub-cli-"));
  mkdirSync(join(root, "dist"), { recursive: true });
  writeFileSync(join(root, "dist", "hub.exe"), "fake exe bytes v2");
  writeFileSync(join(root, "dist", "hubd.exe"), "fake daemon bytes v2");
  writeFileSync(join(root, "dist", "SessionHubTray.exe"), "fake tray bytes v2");
  writeBuildManifest(join(root, "dist"));
  platform = new FakePlatform();
  // A signalled tray quits at once and deletes its tray.pid (tray cases: test/cli/install.test.ts).
  platform.onSignalTrayQuit = () => {
    alive.delete(TRAY_PID);
    rmSync(paths().trayPidFile, { force: true });
    return true;
  };
  platform.onTrayInstanceExists = () => alive.has(TRAY_PID);
  // Every other live pid runs hubd.exe unless `foreign` names another image for it.
  platform.onProcessImagePath = (pid) => {
    if (!alive.has(pid)) return null;
    if (pid === TRAY_PID) return paths().installedTrayExe;
    return foreign.has(pid) ? (foreign.get(pid) ?? null) : paths().installedDaemonExe;
  };
  foreign = new Map();
  starts = new Map();
  platform.onProcessStartTime = (pid) => (alive.has(pid) ? (starts.get(pid) ?? null) : null);
  out = [];
  err = [];
  alive = new Set();
  spawned = [];
  killed = [];
  removedAfterExit = [];
  tuiRuns = [];
  bridgeLinks = [];
  bridgeExit = 0;
  foregroundRuns = 0;
  spawnStartsDaemon = true;
});

afterEach(() => {
  rmSync(root, { recursive: true, force: true });
});

/** Platform calls that change state (process and instance queries left out). */
function changes() {
  return platform.calls.filter(
    (c) => c.method !== "processImagePath" && c.method !== "trayInstanceExists",
  );
}

describe("cliPaths", () => {
  test("derives every path from the data dir and user profile", () => {
    const p = cliPaths({ dataDir: "C:\\L\\SessionHub", userProfile: "C:\\U" });
    expect(p.dbPath).toBe(join("C:\\L\\SessionHub", "hub.db"));
    expect(p.runDir).toBe(join("C:\\L\\SessionHub", "run"));
    expect(p.relayDir).toBe(join("C:\\L\\SessionHub", "relay"));
    expect(p.binDir).toBe(join("C:\\L\\SessionHub", "bin"));
    expect(p.installedExe).toBe(join("C:\\L\\SessionHub", "bin", "hub.exe"));
    expect(p.installedDaemonExe).toBe(join("C:\\L\\SessionHub", "bin", "hubd.exe"));
    expect(p.installedTrayExe).toBe(join("C:\\L\\SessionHub", "bin", "SessionHubTray.exe"));
    expect(p.trayPidFile).toBe(join("C:\\L\\SessionHub", "run", "tray.pid"));
    expect(p.shimDir).toBe(join("C:\\U", ".local", "bin"));
    expect(p.shimPath).toBe(join("C:\\U", ".local", "bin", "hub.cmd"));
  });

  test("shim content runs the installed exe with all args", () => {
    expect(SHIM_CONTENT).toBe('@"%LOCALAPPDATA%\\SessionHub\\bin\\hub.exe" %*\r\n');
  });
});

describe("parsing, usage and exit codes", () => {
  test("--version prints the package version", async () => {
    expect(await runCli(["--version"], makeDeps())).toBe(0);
    expect(out).toEqual(["9.8.7"]);
  });

  test("--help and help print usage with exit 0", async () => {
    expect(await runCli(["--help"], makeDeps())).toBe(0);
    expect(out.join("\n")).toBe(USAGE);
    out = [];
    expect(await runCli(["help"], makeDeps())).toBe(0);
    expect(out.join("\n")).toBe(USAGE);
  });

  test.each([
    [["bogus"]],
    [["daemon"]],
    [["daemon", "restart"]],
    [["daemon", "status", "extra"]],
    [["install", "--force"]],
    [["uninstall", "--purge", "--purge"]],
    [["uninstall", "--keep-data", "--purge"]],
    [["uninstall", "--all"]],
    [["--version", "x"]],
    [["bridge", "x"]],
  ])("%p is a usage error: usage on stderr, exit 2", async (argv) => {
    expect(await runCli(argv, makeDeps())).toBe(2);
    expect(err.join("\n")).toContain(USAGE);
    expect(out).toEqual([]);
    expect(platform.calls).toEqual([]);
    expect(spawned).toEqual([]);
  });

  test("an unexpected error prints its message and exits 1", async () => {
    const deps = makeDeps({
      runDaemonForeground: async () => {
        throw new Error("data dir is not private");
      },
    });
    expect(await runCli(["daemon", "run"], deps)).toBe(1);
    expect(err.join("\n")).toContain("data dir is not private");
  });
});

describe("daemon status", () => {
  test("RUNNING pid=<n> exit 0 when the lock pid is alive", async () => {
    writeLock(77);
    alive.add(77);
    expect(await runCli(["daemon", "status"], makeDeps())).toBe(0);
    expect(out).toEqual(["RUNNING pid=77"]);
  });

  test("STOPPED exit 3 when there is no lock", async () => {
    expect(await runCli(["daemon", "status"], makeDeps())).toBe(3);
    expect(out).toEqual(["STOPPED"]);
  });

  test("STOPPED exit 3 when the lock pid now runs another program", async () => {
    writeLock(77);
    alive.add(77);
    foreign.set(77, OTHER_IMAGE);
    expect(await runCli(["daemon", "status"], makeDeps())).toBe(3);
    expect(out).toEqual(["STOPPED"]);
  });

  test("hub.exe, and a bun.exe or unreadable pid that may be a daemon, all count as running", async () => {
    writeLock(77);
    alive.add(77);
    foreign.set(77, "D:\\x\\dist\\HUB.EXE");
    expect(await runCli(["daemon", "status"], makeDeps())).toBe(0);
    foreign.set(77, "C:\\Users\\u\\.bun\\bin\\bun.exe");
    expect(await runCli(["daemon", "status"], makeDeps())).toBe(0);
    expect(await runCli(["daemon", "status"], makeDeps({ compiled: false }))).toBe(0);
    foreign.set(77, null);
    expect(await runCli(["daemon", "status"], makeDeps())).toBe(0);
    expect(out).toEqual(["RUNNING pid=77", "RUNNING pid=77", "RUNNING pid=77", "RUNNING pid=77"]);
  });

  test("STOPPED exit 3 when the lock pid is dead or the lock is garbage", async () => {
    writeLock(77);
    expect(await runCli(["daemon", "status"], makeDeps())).toBe(3);
    writeFileSync(lockPath(), "not-a-pid");
    expect(await runCli(["daemon", "status"], makeDeps())).toBe(3);
    expect(out).toEqual(["STOPPED", "STOPPED"]);
  });
});

describe("daemon start", () => {
  test("already running: prints ALREADY RUNNING pid=<n> and spawns nothing", async () => {
    writeLock(55);
    alive.add(55);
    expect(await runCli(["daemon", "start"], makeDeps())).toBe(0);
    expect(out).toEqual(["ALREADY RUNNING pid=55"]);
    expect(spawned).toEqual([]);
  });

  test("a lock naming another program's pid: starts the daemon", async () => {
    writeLock(55);
    alive.add(55);
    foreign.set(55, OTHER_IMAGE);
    expect(await runCli(["daemon", "start"], makeDeps())).toBe(0);
    expect(spawned).toEqual([[join(root, "dist", "hubd.exe")]]);
    expect(out).toEqual([`STARTED pid=${DAEMON_PID}`]);
  });

  test("not running: spawns the daemon argv and waits for the lock pid", async () => {
    expect(await runCli(["daemon", "start"], makeDeps())).toBe(0);
    expect(spawned).toEqual([[join(root, "dist", "hubd.exe")]]);
    expect(out).toEqual([`STARTED pid=${DAEMON_PID}`]);
  });

  test("daemonArgvFor: hubd.exe next to the compiled exe, else self, bun script in dev", () => {
    // Built with the platform's own separator, so dirname works the same on the Linux CI runner.
    const app = join(tmpdir(), "app");
    const execPath = join(app, "hub.exe");
    const hubd = join(app, "hubd.exe");
    const base = { execPath, compiled: true, entryPath: "C:\\src\\cli.ts" };
    expect(daemonArgvFor({ ...base, exists: (p) => p === hubd })).toEqual([hubd]);
    expect(daemonArgvFor({ ...base, exists: () => false })).toEqual([execPath, "daemon", "run"]);
    expect(
      daemonArgvFor({ ...base, execPath: "C:\\bun.exe", compiled: false, exists: () => true }),
    ).toEqual(["C:\\bun.exe", "C:\\src\\cli.ts", "daemon", "run"]);
  });

  test("hubdArgv: no args runs the daemon; only daemon and --version pass", () => {
    expect(hubdArgv([])).toEqual(["daemon", "run"]);
    expect(hubdArgv(["daemon", "status"])).toEqual(["daemon", "status"]);
    expect(hubdArgv(["--version"])).toEqual(["--version"]);
    expect(hubdArgv(["install"])).toBeNull();
    expect(hubdArgv(["uninstall", "--purge"])).toBeNull();
    expect(hubdArgv(["bridge"])).toBeNull();
  });

  test("daemon never takes the lock: FAILED exit 1 pointing at the log", async () => {
    spawnStartsDaemon = false;
    let slept = 0;
    const deps = makeDeps({
      sleep: async (ms) => {
        slept += ms;
      },
    });
    expect(await runCli(["daemon", "start"], deps)).toBe(1);
    expect(err.join("\n")).toContain("FAILED");
    expect(err.join("\n")).toContain(join(paths().runDir, "daemon.log"));
    expect(slept).toBeGreaterThanOrEqual(5_000);
    expect(slept).toBeLessThan(5_500);
  });
});

describe("daemon stop", () => {
  test("not running prints STOPPED and kills nothing", async () => {
    expect(await runCli(["daemon", "stop"], makeDeps())).toBe(0);
    expect(out).toEqual(["STOPPED"]);
    expect(killed).toEqual([]);
  });

  test("kills the lock pid tree and removes its stale lock", async () => {
    writeLock(91);
    alive.add(91);
    expect(await runCli(["daemon", "stop"], makeDeps())).toBe(0);
    expect(killed).toEqual([91]);
    expect(out).toEqual(["STOPPED"]);
    expect(existsSync(lockPath())).toBe(false);
  });

  test("a deliberate stop leaves a marker the bridge reads; a start clears it (RES-L1)", async () => {
    writeLock(91);
    alive.add(91);
    expect(await runCli(["daemon", "stop"], makeDeps())).toBe(0);
    expect(existsSync(join(paths().runDir, DAEMON_STOPPED_FILE))).toBe(true);
    await runCli(["bridge"], makeDeps());
    expect((bridgeLinks[0] as BridgeLink).stopRequested()).toBe(true);
    expect(await runCli(["daemon", "start"], makeDeps())).toBe(0);
    expect(existsSync(join(paths().runDir, DAEMON_STOPPED_FILE))).toBe(false);
    expect((bridgeLinks[0] as BridgeLink).stopRequested()).toBe(false);
  });

  test("never kills a lock pid that now runs another program", async () => {
    writeLock(91);
    alive.add(91);
    foreign.set(91, OTHER_IMAGE);
    expect(await runCli(["daemon", "stop"], makeDeps())).toBe(0);
    expect(killed).toEqual([]);
    expect(alive.has(91)).toBe(true);
    expect(out).toEqual(["STOPPED"]);
  });

  test("a live lock pid whose image cannot be read is held: no second daemon, never killed", async () => {
    writeLock(91);
    alive.add(91);
    foreign.set(91, null);
    expect(await runCli(["daemon", "start"], makeDeps())).toBe(0);
    expect(out).toEqual(["ALREADY RUNNING pid=91"]);
    expect(spawned).toEqual([]);
    expect(await runCli(["daemon", "stop"], makeDeps())).toBe(1);
    expect(err.join("\n")).toContain("cannot be verified as the daemon");
    expect(killed).toEqual([]);
    expect(alive.has(91)).toBe(true);
    expect(existsSync(lockPath())).toBe(true);
  });

  test("a bun.exe lock pid seen from the compiled exe is held and never killed; from source it is ours", async () => {
    writeLock(91);
    alive.add(91);
    foreign.set(91, "C:\\Users\\u\\.bun\\bin\\bun.exe");
    expect(await runCli(["daemon", "start"], makeDeps())).toBe(0);
    expect(spawned).toEqual([]);
    expect(await runCli(["daemon", "stop"], makeDeps())).toBe(1);
    expect(killed).toEqual([]);
    expect(await runCli(["daemon", "stop"], makeDeps({ compiled: false }))).toBe(0);
    expect(killed).toEqual([91]);
  });

  test("a lock pid alive with another start time is stale: never killed, a new daemon starts", async () => {
    writeLock(91, T0);
    alive.add(91);
    starts.set(91, T0 + 60_000); // the pid was reused, here by a long-lived bun.exe
    foreign.set(91, "C:\\Users\\u\\.bun\\bin\\bun.exe");
    expect(await runCli(["daemon", "status"], makeDeps())).toBe(3);
    expect(await runCli(["daemon", "stop"], makeDeps())).toBe(0);
    expect(killed).toEqual([]);
    expect(alive.has(91)).toBe(true);
    expect(await runCli(["daemon", "start"], makeDeps())).toBe(0);
    expect(spawned).toEqual([[join(root, "dist", "hubd.exe")]]);
    expect(out).toEqual(["STOPPED", "STOPPED", `STARTED pid=${DAEMON_PID}`]);
  });

  test("a matching start time is our daemon whatever its image: held, and stoppable", async () => {
    writeLock(91, T0);
    alive.add(91);
    starts.set(91, T0 + 1_500);
    foreign.set(91, "C:\\Users\\u\\.bun\\bin\\bun.exe"); // a daemon run from source
    expect(await runCli(["daemon", "start"], makeDeps())).toBe(0);
    expect(spawned).toEqual([]);
    expect(await runCli(["daemon", "stop"], makeDeps())).toBe(0);
    expect(killed).toEqual([91]);
    expect(existsSync(lockPath())).toBe(false);
  });

  test("a start time that cannot be read is held: no second daemon, never killed", async () => {
    writeLock(91, T0);
    alive.add(91); // no entry in `starts`: the query fails (elevated, protected)
    expect(await runCli(["daemon", "start"], makeDeps())).toBe(0);
    expect(out).toEqual(["ALREADY RUNNING pid=91"]);
    expect(spawned).toEqual([]);
    expect(await runCli(["daemon", "stop"], makeDeps())).toBe(1);
    expect(killed).toEqual([]);
  });

  test("ours by start time but not a daemon image: the guard before the kill refuses", async () => {
    writeLock(91, T0);
    alive.add(91);
    starts.set(91, T0);
    foreign.set(91, OTHER_IMAGE);
    expect(await runCli(["daemon", "stop"], makeDeps())).toBe(1);
    expect(killed).toEqual([]);
  });

  test("a process that survives the kill: FAILED exit 1", async () => {
    writeLock(91);
    alive.add(91);
    const deps = makeDeps({
      killTree: async (pid) => {
        killed.push(pid);
      },
    });
    expect(await runCli(["daemon", "stop"], deps)).toBe(1);
    expect(err.join("\n")).toContain("FAILED");
    expect(existsSync(lockPath())).toBe(true);
  });
});

describe("daemon run", () => {
  test("delegates to the foreground runner and exits 0 when it stops", async () => {
    expect(await runCli(["daemon", "run"], makeDeps())).toBe(0);
    expect(foregroundRuns).toBe(1);
  });

  test("prints ALREADY RUNNING exit 0 when the runner reports a live lock", async () => {
    const deps = makeDeps({ runDaemonForeground: async () => "already-running" });
    expect(await runCli(["daemon", "run"], deps)).toBe(0);
    expect(out).toEqual(["ALREADY RUNNING"]);
  });
});

describe("hub (no args)", () => {
  test("starts the daemon when stopped, then opens the TUI without a warning", async () => {
    expect(await runCli([], makeDeps())).toBe(0);
    expect(spawned.length).toBe(1);
    expect(tuiRuns).toEqual([{}]);
  });

  test("daemon already running: opens the TUI, spawns nothing", async () => {
    writeLock(12);
    alive.add(12);
    expect(await runCli([], makeDeps())).toBe(0);
    expect(spawned).toEqual([]);
    expect(tuiRuns).toEqual([{}]);
  });

  test("daemon start fails: still opens the TUI with a footer warning", async () => {
    spawnStartsDaemon = false;
    expect(await runCli([], makeDeps())).toBe(0);
    expect(tuiRuns.length).toBe(1);
    expect(tuiRuns[0]?.warning).toContain("daemon is not running");
    expect(tuiRuns[0]?.warning).toContain("daemon.log");
  });

  test("creates the private data dir first when it does not exist", async () => {
    await runCli([], makeDeps());
    expect(platform.calls[0]).toEqual({ method: "ensurePrivateDir", args: [paths().dataDir] });
  });
});

describe("bridge", () => {
  test("runs the bridge, prints nothing on stdout, returns its exit code", async () => {
    bridgeExit = 0;
    expect(await runCli(["bridge"], makeDeps())).toBe(0);
    expect(bridgeLinks).toHaveLength(1);
    expect(out).toEqual([]);
    expect(spawned).toEqual([]);
  });

  test("daemonRunning follows the lock pid", async () => {
    await runCli(["bridge"], makeDeps());
    const link = bridgeLinks[0] as BridgeLink;
    expect(link.daemonRunning()).toBe(false);
    writeLock(31);
    expect(link.daemonRunning()).toBe(false);
    alive.add(31);
    expect(link.daemonRunning()).toBe(true);
    foreign.set(31, OTHER_IMAGE);
    expect(link.daemonRunning()).toBe(false);
  });

  test("startDaemon starts it hidden, creating the private data dir first", async () => {
    await runCli(["bridge"], makeDeps());
    const link = bridgeLinks[0] as BridgeLink;
    expect(await link.startDaemon()).toBe(true);
    expect(platform.calls[0]).toEqual({ method: "ensurePrivateDir", args: [paths().dataDir] });
    expect(spawned).toEqual([[join(root, "dist", "hubd.exe")]]);
    expect(link.daemonRunning()).toBe(true);
    expect(await link.startDaemon()).toBe(true);
    expect(spawned).toHaveLength(1);
    expect(out).toEqual([]);
  });

  test("startDaemon reports false when the daemon does not start", async () => {
    spawnStartsDaemon = false;
    await runCli(["bridge"], makeDeps());
    expect(await (bridgeLinks[0] as BridgeLink).startDaemon()).toBe(false);
    expect(out).toEqual([]);
  });

  test("usage lists the bridge as internal", () => {
    expect(USAGE).toContain(
      "  hub bridge              (internal) data link for the tray app over stdin/stdout",
    );
  });
});

describe("install", () => {
  test("refuses outside the compiled exe", async () => {
    const deps = makeDeps({ compiled: false, daemonArgv: ["bun", "src/cli.ts", "daemon", "run"] });
    expect(await runCli(["install"], deps)).toBe(1);
    expect(err.join("\n")).toContain("run from dist\\hub.exe");
    expect(platform.calls).toEqual([]);
  });

  test("refuses when hubd.exe is not next to hub.exe", async () => {
    rmSync(join(root, "dist", "hubd.exe"));
    expect(await runCli(["install"], makeDeps())).toBe(1);
    expect(err.join("\n")).toContain(join(root, "dist", "hubd.exe"));
    expect(platform.calls).toEqual([]);
    expect(existsSync(paths().binDir)).toBe(false);
  });

  test("copies the exes, writes the shim, sets autostart, starts the daemon and the tray", async () => {
    const p = paths();
    expect(await runCli(["install"], makeDeps())).toBe(0);
    expect(readFileSync(p.installedExe, "utf8")).toBe("fake exe bytes v2");
    expect(readFileSync(p.installedDaemonExe, "utf8")).toBe("fake daemon bytes v2");
    expect(existsSync(`${p.installedExe}.new`)).toBe(false);
    expect(existsSync(`${p.installedDaemonExe}.new`)).toBe(false);
    expect(readFileSync(p.shimPath, "utf8")).toBe(SHIM_CONTENT);
    expect(platform.calls).toContainEqual({ method: "ensurePrivateDir", args: [p.dataDir] });
    expect(platform.calls).toContainEqual({
      method: "setAutostart",
      args: ["SessionHubDaemon", p.installedDaemonExe],
    });
    expect(spawned).toEqual([[p.installedDaemonExe], [p.installedTrayExe]]);
    const text = out.join("\n");
    expect(text).toContain(p.installedExe);
    expect(text).toContain(p.shimPath);
    expect(text).toContain(p.dataDir);
  });

  test("refuses a foreign hub.cmd and changes nothing", async () => {
    const p = paths();
    mkdirSync(p.shimDir, { recursive: true });
    writeFileSync(p.shimPath, "@echo someone else's hub\r\n");
    expect(await runCli(["install"], makeDeps())).toBe(1);
    expect(err.join("\n")).toContain(p.shimPath);
    expect(readFileSync(p.shimPath, "utf8")).toBe("@echo someone else's hub\r\n");
    expect(existsSync(p.installedExe)).toBe(false);
    expect(platform.calls).toEqual([]);
    expect(spawned).toEqual([]);
  });

  test("refuses a hub.exe in the shim folder", async () => {
    const p = paths();
    mkdirSync(p.shimDir, { recursive: true });
    writeFileSync(join(p.shimDir, "hub.exe"), "x");
    expect(await runCli(["install"], makeDeps())).toBe(1);
    expect(err.join("\n")).toContain(join(p.shimDir, "hub.exe"));
    expect(platform.calls).toEqual([]);
  });

  test("is idempotent with our own shim and replaces an older exe", async () => {
    const p = paths();
    expect(await runCli(["install"], makeDeps())).toBe(0);
    writeFileSync(join(root, "dist", "hub.exe"), "fake exe bytes v3");
    writeFileSync(join(root, "dist", "hubd.exe"), "fake daemon bytes v3");
    writeBuildManifest(join(root, "dist"));
    expect(await runCli(["install"], makeDeps())).toBe(0);
    expect(readFileSync(p.installedExe, "utf8")).toBe("fake exe bytes v3");
    expect(readFileSync(p.installedDaemonExe, "utf8")).toBe("fake daemon bytes v3");
    expect(readFileSync(p.shimPath, "utf8")).toBe(SHIM_CONTENT);
    // the second run stopped the first run's tray and daemon before replacing the exes, then
    // restarted both
    expect(killed).toEqual([DAEMON_PID]);
    expect(spawned.length).toBe(4);
    expect(err).toEqual([]);
  });

  test("run from the installed exe: no copy, daemon left running", async () => {
    const p = paths();
    mkdirSync(p.binDir, { recursive: true });
    writeFileSync(p.installedExe, "installed bytes");
    writeFileSync(p.installedDaemonExe, "installed daemon bytes");
    writeFileSync(p.installedTrayExe, "installed tray bytes");
    writeLock(33);
    alive.add(33);
    const deps = makeDeps({ exePath: p.installedExe, daemonArgv: [p.installedDaemonExe] });
    expect(await runCli(["install"], deps)).toBe(0);
    expect(readFileSync(p.installedExe, "utf8")).toBe("installed bytes");
    expect(killed).toEqual([]);
    expect(spawned).toEqual([[p.installedTrayExe]]);
    expect(out.join("\n")).toContain("ALREADY RUNNING pid=33");
  });
});

describe("uninstall", () => {
  async function installed(): Promise<void> {
    expect(await runCli(["install"], makeDeps())).toBe(0);
    out = [];
    platform.calls.length = 0;
  }

  test("--keep-data: clears autostart, stops the daemon, removes shim and bin, keeps data", async () => {
    await installed();
    const p = paths();
    writeFileSync(p.dbPath, "db");
    expect(await runCli(["uninstall", "--keep-data"], makeDeps())).toBe(0);
    expect(changes()).toEqual([
      { method: "signalTrayQuit", args: [] },
      { method: "clearAutostart", args: ["SessionHubDaemon"] },
      { method: "clearAutostart", args: ["SessionHubTray"] },
    ]);
    expect(killed).toEqual([DAEMON_PID]);
    expect(existsSync(p.shimPath)).toBe(false);
    expect(existsSync(p.binDir)).toBe(false);
    expect(existsSync(p.dbPath)).toBe(true);
    expect(out.join("\n")).toContain(`data: kept ${p.dataDir}`);
  });

  test("by default also deletes the data dir (SH42)", async () => {
    await installed();
    const p = paths();
    writeFileSync(p.dbPath, "db");
    expect(await runCli(["uninstall"], makeDeps())).toBe(0);
    expect(existsSync(p.dataDir)).toBe(false);
    expect(existsSync(p.shimPath)).toBe(false);
    expect(out.join("\n")).not.toContain("data: kept");
  });

  test("uninstall marks the daemon stopped, so a tray that would not quit cannot restart it", async () => {
    await installed();
    expect(await runCli(["uninstall", "--keep-data"], makeDeps())).toBe(0);
    expect(existsSync(join(paths().runDir, DAEMON_STOPPED_FILE))).toBe(true);
  });

  test("--purge also removes the data dir", async () => {
    await installed();
    const p = paths();
    expect(await runCli(["uninstall", "--purge"], makeDeps())).toBe(0);
    expect(existsSync(p.dataDir)).toBe(false);
  });

  test("leaves a foreign shim in place", async () => {
    const p = paths();
    mkdirSync(p.shimDir, { recursive: true });
    writeFileSync(p.shimPath, "@echo other\r\n");
    expect(await runCli(["uninstall"], makeDeps())).toBe(0);
    expect(readFileSync(p.shimPath, "utf8")).toBe("@echo other\r\n");
    expect(out.join("\n")).toContain("not created by Session Hub");
  });

  test("run from the installed exe: bin removal is deferred until exit", async () => {
    await installed();
    const p = paths();
    const deps = makeDeps({ exePath: p.installedExe, daemonArgv: [p.installedDaemonExe] });
    expect(await runCli(["uninstall", "--purge"], deps)).toBe(0);
    expect(existsSync(p.installedExe)).toBe(true);
    expect(removedAfterExit).toEqual([[p.dataDir]]);
    expect(existsSync(p.dbPath)).toBe(false);
  });

  test("a bin dir still locked by the killed daemon is removed after exit", async () => {
    await installed();
    const p = paths();
    const fs = {
      ...nodeFs,
      remove: (path: string) => {
        if (path === p.binDir) throw new Error("EBUSY: resource busy or locked");
        nodeFs.remove(path);
      },
    };
    expect(await runCli(["uninstall", "--keep-data"], makeDeps({ fs }))).toBe(0);
    expect(removedAfterExit).toEqual([[p.binDir]]);
    expect(existsSync(p.shimPath)).toBe(false);
  });

  test("works when nothing is installed", async () => {
    expect(await runCli(["uninstall"], makeDeps())).toBe(0);
    expect(changes()).toEqual([
      { method: "signalTrayQuit", args: [] },
      { method: "clearAutostart", args: ["SessionHubDaemon"] },
      { method: "clearAutostart", args: ["SessionHubTray"] },
    ]);
  });
});

describe("daemonRun (foreground)", () => {
  function runDeps(over: Partial<Parameters<typeof daemonRun>[0]> = {}) {
    const p = paths();
    const runs: DaemonDeps[] = [];
    const handlers = new Map<string, (e?: unknown) => void>();
    const deps: Parameters<typeof daemonRun>[0] = {
      platform,
      paths: p,
      profiles: [],
      cli: { agents: async () => [] },
      now: () => 1_000,
      daemonIdentity: (lock) =>
        identifyDaemon(
          lock,
          {
            isAlive: (q) => alive.has(q),
            startTime: (q) => (alive.has(q) ? (starts.get(q) ?? null) : null),
            imagePath: (q) =>
              foreign.has(q) ? (foreign.get(q) ?? null) : paths().installedDaemonExe,
          },
          false,
        ),
      runDaemon: async (d) => {
        runs.push(d);
        await new Promise<void>((r) => d.signal.addEventListener("abort", () => r()));
      },
      onRecords: () => () => {},
      expireSends: () => 0,
      signals: {
        on: (sig, fn) => handlers.set(sig, fn),
        off: (sig) => handlers.delete(sig),
      },
      ...over,
    };
    return { deps, runs, handlers };
  }

  test("prints nothing and returns already-running when a live pid holds the lock", async () => {
    writeLock(66);
    alive.add(66);
    const { deps, runs } = runDeps();
    expect(await daemonRun(deps)).toBe("already-running");
    expect(runs).toEqual([]);
    expect(platform.calls).toEqual([]);
  });

  test("a lock naming another program's pid does not stop the daemon from starting", async () => {
    writeLock(66);
    alive.add(66);
    foreign.set(66, OTHER_IMAGE);
    const { deps, runs, handlers } = runDeps();
    const done = daemonRun(deps);
    await Bun.sleep(20);
    expect(runs.length).toBe(1);
    expect(runs[0]?.daemonIdentity?.({ pid: 66, startedAt: null })).toBe("foreign");
    handlers.get("SIGTERM")?.();
    expect(await done).toBe("stopped");
  });

  test("a pid reused by a later process (another start time) does not stop the daemon from starting", async () => {
    writeLock(66, T0);
    alive.add(66);
    starts.set(66, T0 + 60_000);
    foreign.set(66, "C:\\Users\\u\\.bun\\bin\\bun.exe");
    const { deps, runs, handlers } = runDeps();
    const done = daemonRun(deps);
    await Bun.sleep(20);
    expect(runs.length).toBe(1);
    handlers.get("SIGTERM")?.();
    expect(await done).toBe("stopped");
  });

  test("a lock naming a live pid that cannot be verified means already-running", async () => {
    for (const image of [null, "C:\\Users\\u\\.bun\\bin\\bun.exe"]) {
      writeLock(66);
      alive.add(66);
      foreign.set(66, image);
      const { deps, runs } = runDeps();
      expect(await daemonRun(deps)).toBe("already-running");
      expect(runs).toEqual([]);
    }
  });

  test("a store that cannot open is logged before the error is thrown (RES-L2)", async () => {
    const p = paths();
    mkdirSync(p.dbPath, { recursive: true }); // a folder where the index file should be
    const { deps } = runDeps();
    await expect(daemonRun(deps)).rejects.toThrow();
    const log = readFileSync(join(p.runDir, "daemon.log"), "utf8");
    expect(log).toContain('"event":"store_open_failed"');
  });

  test("an uncaught error or rejection is logged and stops the daemon cleanly (RES-L2)", async () => {
    const p = paths();
    const { deps, handlers } = runDeps();
    const done = daemonRun(deps);
    await Bun.sleep(20);
    expect(handlers.has("uncaughtException")).toBe(true);
    expect(handlers.has("unhandledRejection")).toBe(true);
    (handlers.get("unhandledRejection") as (e: unknown) => void)(new TypeError("boom"));
    expect(await done).toBe("stopped");
    expect(handlers.size).toBe(0);
    const log = readFileSync(join(p.runDir, "daemon.log"), "utf8");
    expect(log).toContain('"event":"uncaught"');
    expect(log).toContain('"err":"TypeError"');
  });

  test("maps a lost lock race inside runDaemon to already-running", async () => {
    const { deps } = runDeps({
      runDaemon: async () => {
        throw new Error("daemon already running");
      },
    });
    expect(await daemonRun(deps)).toBe("already-running");
  });

  test("a daemon that runs (login start included) clears the stop marker", async () => {
    const p = paths();
    mkdirSync(p.runDir, { recursive: true });
    writeFileSync(join(p.runDir, DAEMON_STOPPED_FILE), "");
    const { deps, handlers } = runDeps();
    const done = daemonRun(deps);
    await Bun.sleep(20);
    expect(existsSync(join(p.runDir, DAEMON_STOPPED_FILE))).toBe(false);
    handlers.get("SIGTERM")?.();
    expect(await done).toBe("stopped");
  });

  test("sets up dirs, runs the collector until a signal, then cleans up", async () => {
    const p = paths();
    const { deps, runs, handlers } = runDeps();
    const done = daemonRun(deps);
    await Bun.sleep(20);
    expect(platform.calls[0]).toEqual({ method: "ensurePrivateDir", args: [p.dataDir] });
    expect(existsSync(p.runDir)).toBe(true);
    expect(existsSync(p.relayDir)).toBe(true);
    expect(existsSync(p.dbPath)).toBe(true);
    expect(runs.length).toBe(1);
    expect(runs[0]?.dataDir).toBe(p.dataDir);
    expect(runs[0]?.relayDir).toBe(p.relayDir);
    expect(typeof runs[0]?.onRecords).toBe("function");
    expect([...handlers.keys()].sort()).toEqual([
      "SIGBREAK",
      "SIGINT",
      "SIGTERM",
      "uncaughtException",
      "unhandledRejection",
    ]);
    handlers.get("SIGTERM")?.();
    expect(await done).toBe("stopped");
    expect(handlers.size).toBe(0);
    // the store was closed: the db file can be removed on Windows
    rmSync(p.dataDir, { recursive: true, force: true });
    expect(existsSync(p.dataDir)).toBe(false);
  });
});
