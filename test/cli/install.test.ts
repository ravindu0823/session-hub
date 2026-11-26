import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";
import { type CliDeps, type CliFs, cliPaths, nodeFs, runCli } from "../../src/cli/commands.ts";
import { MANIFEST_FILE } from "../../src/cli/manifest.ts";
import { LOCK_FILE } from "../../src/daemon/lock.ts";
import { FakePlatform, type PlatformCall } from "../../src/platform/fake.ts";
import { TEST_COMMIT, writeBuildManifest } from "../fakes/build-manifest.ts";

// Install and uninstall with the tray app. Everything is fake: temp dirs, a recording platform (no
// registry, no real quit event, no real process queries) and a fake clock, so no test can touch the
// real tray or daemon. TRAY_PID is always the installed tray: its image is <bin>\SessionHubTray.exe and
// it holds the instance mutex while it is alive.

const DAEMON_PID = 4242;
const TRAY_PID = 5151;
const OTHER_IMAGE = "C:\\Windows\\System32\\notepad.exe";

let root: string;
let platform: FakePlatform;
let out: string[];
let err: string[];
let alive: Set<number>;
/** Executable image of each fake pid. */
let images: Map<number, string>;
let spawned: string[][];
/** Options of each spawnDetached call, in order. */
let spawnOpts: ({ gui?: boolean; cwd?: string } | undefined)[];
let killed: number[];
/** signal, kill:<pid>, copy:<file>, spawn:<file>, set:<name>, clear:<name>, in call order. */
let events: string[];
let clock: number;
/** Fake-clock time at which a signalled tray exits cleanly; null: it ignores the signal. */
let trayExitAt: number | null;
/** Pids that taskkill cannot end. */
let unkillable: Set<number>;
let trayStarts: boolean;
/** Renames still to fail, by target file name (Infinity: always). */
let renameFailures: Map<string, number>;

function paths() {
  return cliPaths({ dataDir: join(root, "data"), userProfile: join(root, "home") });
}

function writePid(file: string, pid: number): void {
  mkdirSync(paths().runDir, { recursive: true });
  writeFileSync(file, String(pid));
}

function trayRunning(pid = TRAY_PID, image = paths().installedTrayExe): void {
  writePid(paths().trayPidFile, pid);
  alive.add(pid);
  images.set(pid, image);
}

function daemonRunning(): void {
  writePid(join(paths().runDir, LOCK_FILE), DAEMON_PID);
  alive.add(DAEMON_PID);
  images.set(DAEMON_PID, paths().installedDaemonExe);
}

function advance(ms: number): void {
  clock += ms;
  if (trayExitAt !== null && clock >= trayExitAt && alive.has(TRAY_PID)) {
    alive.delete(TRAY_PID);
    rmSync(paths().trayPidFile, { force: true }); // a clean exit deletes tray.pid
  }
}

const recordingFs: CliFs = {
  ...nodeFs,
  copy: (src, dst) => {
    events.push(`copy:${basename(dst)}`);
    nodeFs.copy(src, dst);
  },
  rename: (from, to) => {
    const left = renameFailures.get(basename(to)) ?? 0;
    if (left > 0) {
      renameFailures.set(basename(to), left - 1);
      throw new Error("EBUSY: resource busy or locked");
    }
    nodeFs.rename(from, to);
  },
};

function makeDeps(over: Partial<CliDeps> = {}): CliDeps {
  const p = paths();
  return {
    platform,
    paths: p,
    version: "9.8.7",
    exePath: join(root, "dist", "hub.exe"),
    compiled: true,
    daemonArgv: [join(root, "dist", "hubd.exe")],
    fs: recordingFs,
    out: (l) => out.push(l),
    err: (l) => err.push(l),
    spawnDetached: (argv, opts) => {
      spawned.push(argv);
      spawnOpts.push(opts);
      const exe = argv[0] ?? "";
      events.push(`spawn:${basename(exe)}`);
      if (exe === p.installedDaemonExe) daemonRunning();
      else if (exe === p.installedTrayExe && trayStarts) trayRunning();
    },
    removeAfterExit: () => {},
    isAlive: (pid) => alive.has(pid),
    killTree: async (pid) => {
      killed.push(pid);
      events.push(`kill:${pid}`);
      if (!unkillable.has(pid)) alive.delete(pid);
    },
    runDaemonForeground: async () => "stopped",
    runTui: async () => {},
    runBridge: async () => 0,
    guardArgv: ["hub.exe", "relay-guard"],
    runRelayGuard: async () => 0,
    runMail: async () => 0,
    sleep: async (ms) => advance(ms),
    ...over,
  };
}

beforeEach(() => {
  root = mkdtempSync(join(tmpdir(), "hub-install-"));
  mkdirSync(join(root, "dist"), { recursive: true });
  writeFileSync(join(root, "dist", "hub.exe"), "hub v2");
  writeFileSync(join(root, "dist", "hubd.exe"), "hubd v2");
  writeFileSync(join(root, "dist", "SessionHubTray.exe"), "tray v2");
  writeBuildManifest(join(root, "dist"));
  out = [];
  err = [];
  alive = new Set();
  images = new Map();
  spawned = [];
  spawnOpts = [];
  killed = [];
  events = [];
  clock = 0;
  trayExitAt = null;
  unkillable = new Set();
  trayStarts = true;
  renameFailures = new Map();
  platform = new FakePlatform();
  platform.onSetAutostart = (name) => events.push(`set:${name}`);
  platform.onClearAutostart = (name) => events.push(`clear:${name}`);
  platform.onSignalTrayQuit = () => {
    events.push("signal");
    return alive.has(TRAY_PID);
  };
  platform.onTrayInstanceExists = () => alive.has(TRAY_PID);
  platform.onProcessImagePath = (pid) => (alive.has(pid) ? (images.get(pid) ?? null) : null);
});

afterEach(() => {
  rmSync(root, { recursive: true, force: true });
});

function signals(): number {
  return platform.calls.filter((c) => c.method === "signalTrayQuit").length;
}

/** Platform calls that change state (queries left out). */
function changes(): PlatformCall[] {
  return platform.calls.filter(
    (c) => c.method !== "processImagePath" && c.method !== "trayInstanceExists",
  );
}

describe("cliPaths", () => {
  test("tray exe in bin, tray.pid in run", () => {
    const p = cliPaths({ dataDir: "C:\\L\\SessionHub", userProfile: "C:\\U" });
    expect(p.installedTrayExe).toBe(join("C:\\L\\SessionHub", "bin", "SessionHubTray.exe"));
    expect(p.trayPidFile).toBe(join("C:\\L\\SessionHub", "run", "tray.pid"));
  });
});

describe("install with the tray app", () => {
  test("copies three exes, sets both Run values, starts the daemon then the tray", async () => {
    const p = paths();
    expect(await runCli(["install"], makeDeps())).toBe(0);
    expect(readFileSync(p.installedExe, "utf8")).toBe("hub v2");
    expect(readFileSync(p.installedDaemonExe, "utf8")).toBe("hubd v2");
    expect(readFileSync(p.installedTrayExe, "utf8")).toBe("tray v2");
    expect(existsSync(`${p.installedTrayExe}.new`)).toBe(false);
    expect(platform.calls).toContainEqual({
      method: "setAutostart",
      args: ["SessionHubDaemon", p.installedDaemonExe],
    });
    expect(platform.calls).toContainEqual({
      method: "setAutostart",
      args: ["SessionHubTray", p.installedTrayExe],
    });
    expect(spawned).toEqual([[p.installedDaemonExe], [p.installedTrayExe]]);
    // both run in bin (never pinning the installer's folder); only the daemon starts hidden
    expect(spawnOpts).toEqual([{ cwd: p.binDir }, { gui: true, cwd: p.binDir }]);
    // the quit event is always set first; with no tray running it is a no-op
    expect(events).toEqual([
      "signal",
      "copy:hub.exe.new",
      "copy:hubd.exe.new",
      "copy:SessionHubTray.exe.new",
      "set:SessionHubDaemon",
      "set:SessionHubTray",
      "spawn:hubd.exe",
      "spawn:SessionHubTray.exe",
    ]);
    expect(out.join("\n")).toContain(`tray: STARTED pid=${TRAY_PID}`);
    expect(out.join("\n")).not.toContain("tray: STOPPED");
    expect(err).toEqual([]);
  });

  test("missing dist\\SessionHubTray.exe: error names the build command, nothing changed", async () => {
    rmSync(join(root, "dist", "SessionHubTray.exe"));
    expect(await runCli(["install"], makeDeps())).toBe(1);
    const text = err.join("\n");
    expect(text).toContain(join(root, "dist", "SessionHubTray.exe"));
    expect(text).toContain("bun run build");
    expect(platform.calls).toEqual([]);
    expect(events).toEqual([]);
    expect(existsSync(paths().dataDir)).toBe(false);
  });

  test("a running tray is asked to quit before anything is stopped or copied", async () => {
    trayRunning();
    daemonRunning();
    platform.onSignalTrayQuit = () => {
      events.push("signal");
      trayExitAt = clock + 300;
      return true;
    };
    expect(await runCli(["install"], makeDeps())).toBe(0);
    expect(signals()).toBe(1);
    expect(killed).toEqual([DAEMON_PID]); // the tray quit by itself: never killed
    expect(events.slice(0, 3)).toEqual(["signal", `kill:${DAEMON_PID}`, "copy:hub.exe.new"]);
    expect(clock).toBeLessThan(5_000);
    expect(spawned.at(-1)).toEqual([paths().installedTrayExe]);
    expect(err).toEqual([]);
  });

  test("a tray that ignores the signal is killed after 5 s, then its stale tray.pid dropped", async () => {
    trayRunning();
    let signalledAt = -1;
    let killedAt = -1;
    platform.onSignalTrayQuit = () => {
      events.push("signal");
      signalledAt = clock;
      return true;
    };
    const atFirstCopy: { pidFile?: boolean } = {};
    const deps = makeDeps({
      killTree: async (pid) => {
        if (pid === TRAY_PID) killedAt = clock;
        killed.push(pid);
        events.push(`kill:${pid}`);
        alive.delete(pid);
      },
      fs: {
        ...recordingFs,
        copy: (src, dst) => {
          atFirstCopy.pidFile ??= existsSync(paths().trayPidFile);
          recordingFs.copy(src, dst);
        },
      },
    });
    expect(await runCli(["install"], deps)).toBe(0);
    expect(killedAt - signalledAt).toBeGreaterThanOrEqual(5_000);
    expect(events.slice(0, 3)).toEqual(["signal", `kill:${TRAY_PID}`, "copy:hub.exe.new"]);
    // a killed tray cannot delete its tray.pid: install drops it before copying
    expect(atFirstCopy.pidFile).toBe(false);
    expect(err).toEqual([]);
  });

  test("tray.pid naming another program is never killed, even while a tray holds the event", async () => {
    trayRunning(777, OTHER_IMAGE); // pid reused by some other program
    alive.add(TRAY_PID); // a real tray runs too (its pid is not in tray.pid) and ignores the signal
    images.set(TRAY_PID, paths().installedTrayExe);
    platform.onSignalTrayQuit = () => {
      events.push("signal");
      return true;
    };
    expect(await runCli(["install"], makeDeps())).toBe(1);
    expect(out.join("\n")).toContain("tray.pid points at another program (pid=777)");
    expect(out.join("\n")).toContain("elevated");
    expect(killed).toEqual([]);
    expect(alive.has(777)).toBe(true);
    // the unverified tray did not quit: fail before anything is stopped or copied
    expect(err.join("\n")).toContain("did not quit");
    expect(events.some((e) => e.startsWith("copy:"))).toBe(false);
  });

  test("a stale tray.pid naming a live non-tray process is dropped and the tray started", async () => {
    trayRunning(777, OTHER_IMAGE);
    expect(await runCli(["install"], makeDeps())).toBe(0);
    expect(signals()).toBe(1);
    expect(killed).not.toContain(777);
    expect(alive.has(777)).toBe(true);
    expect(spawned.at(-1)).toEqual([paths().installedTrayExe]);
    expect(readFileSync(paths().trayPidFile, "utf8")).toBe(String(TRAY_PID));
    expect(err).toEqual([]);
  });

  test("tray.pid missing: the tray still gets the quit signal and is waited for", async () => {
    alive.add(TRAY_PID); // running, but tray.pid was deleted
    images.set(TRAY_PID, paths().installedTrayExe);
    platform.onSignalTrayQuit = () => {
      events.push("signal");
      trayExitAt = clock + 400;
      return true;
    };
    expect(await runCli(["install"], makeDeps())).toBe(0);
    expect(killed).toEqual([]);
    expect(clock).toBeGreaterThanOrEqual(400);
    expect(events.slice(0, 2)).toEqual(["signal", "copy:hub.exe.new"]);
    expect(out.join("\n")).toContain("tray: STOPPED");
  });

  test("a tray that survives the kill fails the install before any copy", async () => {
    trayRunning();
    unkillable.add(TRAY_PID);
    expect(await runCli(["install"], makeDeps())).toBe(1);
    expect(err.join("\n")).toContain(`tray pid=${TRAY_PID} did not stop`);
    expect(events.some((e) => e.startsWith("copy:"))).toBe(false);
    expect(existsSync(paths().installedExe)).toBe(false);
    expect(spawned).toEqual([]);
  });

  test("a tray that does not start within 5 s is reported with its log path only", async () => {
    trayStarts = false;
    expect(await runCli(["install"], makeDeps())).toBe(1);
    const text = err.join("\n");
    expect(text).toContain("did not start within 5 s");
    expect(text).toContain(join(paths().runDir, "tray.log"));
    expect(text).not.toContain(".NET");
  });

  test("a second install quits the first install's tray and replaces all three exes", async () => {
    const p = paths();
    platform.onSignalTrayQuit = () => {
      events.push("signal");
      trayExitAt = clock + 100;
      return alive.has(TRAY_PID);
    };
    expect(await runCli(["install"], makeDeps())).toBe(0);
    writeFileSync(join(root, "dist", "hub.exe"), "hub v3");
    writeFileSync(join(root, "dist", "hubd.exe"), "hubd v3");
    writeFileSync(join(root, "dist", "SessionHubTray.exe"), "tray v3");
    writeBuildManifest(join(root, "dist"));
    events = [];
    expect(await runCli(["install"], makeDeps())).toBe(0);
    expect(readFileSync(p.installedExe, "utf8")).toBe("hub v3");
    expect(readFileSync(p.installedDaemonExe, "utf8")).toBe("hubd v3");
    expect(readFileSync(p.installedTrayExe, "utf8")).toBe("tray v3");
    expect(events).toEqual([
      "signal",
      `kill:${DAEMON_PID}`,
      "copy:hub.exe.new",
      "copy:hubd.exe.new",
      "copy:SessionHubTray.exe.new",
      "copy:hub.exe.old",
      "copy:hubd.exe.old",
      "copy:SessionHubTray.exe.old",
      "set:SessionHubDaemon",
      "set:SessionHubTray",
      "spawn:hubd.exe",
      "spawn:SessionHubTray.exe",
    ]);
    expect(killed).toEqual([DAEMON_PID]);
    expect(err).toEqual([]);
  });

  test("a locked exe is retried until it frees up", async () => {
    renameFailures.set("hub.exe", 5);
    expect(await runCli(["install"], makeDeps())).toBe(0);
    expect(readFileSync(paths().installedExe, "utf8")).toBe("hub v2");
    expect(clock).toBeGreaterThanOrEqual(500);
    expect(err).toEqual([]);
  });

  test("a failed swap restarts the installed daemon and tray, exit 1", async () => {
    const p = paths();
    platform.onSignalTrayQuit = () => {
      events.push("signal");
      trayExitAt = clock + 100;
      return alive.has(TRAY_PID);
    };
    expect(await runCli(["install"], makeDeps())).toBe(0);
    writeFileSync(join(root, "dist", "hub.exe"), "hub v3");
    writeBuildManifest(join(root, "dist"));
    renameFailures.set("hub.exe", Number.POSITIVE_INFINITY);
    events = [];
    err = [];
    const before = clock;
    expect(await runCli(["install"], makeDeps())).toBe(1);
    expect(clock - before).toBeGreaterThanOrEqual(10_000);
    const text = err.join("\n");
    expect(text).toContain(`cannot replace ${p.installedExe}`);
    expect(text).toContain("tray app");
    expect(existsSync(`${p.installedExe}.new`)).toBe(false);
    expect(readFileSync(p.installedExe, "utf8")).toBe("hub v2");
    // stopped for the swap, then started again from what is installed
    expect(events).toEqual([
      "signal",
      `kill:${DAEMON_PID}`,
      "copy:hub.exe.new",
      "copy:hubd.exe.new",
      "copy:SessionHubTray.exe.new",
      "copy:hub.exe.old",
      "copy:hubd.exe.old",
      "copy:SessionHubTray.exe.old",
      "spawn:hubd.exe",
      "spawn:SessionHubTray.exe",
    ]);
    expect(existsSync(`${p.installedDaemonExe}.new`)).toBe(false);
    expect(existsSync(`${p.installedExe}.old`)).toBe(false);
    expect(alive.has(DAEMON_PID)).toBe(true);
    expect(alive.has(TRAY_PID)).toBe(true);
  });

  test("a failed copy restarts the installed daemon and tray, exit 1", async () => {
    const p = paths();
    platform.onSignalTrayQuit = () => {
      events.push("signal");
      trayExitAt = clock + 100;
      return alive.has(TRAY_PID);
    };
    expect(await runCli(["install"], makeDeps())).toBe(0);
    events = [];
    err = [];
    const fs: CliFs = {
      ...recordingFs,
      copy: (src, dst) => {
        events.push(`copy:${basename(dst)}`);
        if (basename(dst) === "hub.exe.new") throw new Error("ENOSPC: no space left on device");
        nodeFs.copy(src, dst);
      },
      // removing the missing .new fails too: it must not hide the copy error or skip the restart
      remove: (path) => {
        if (path.endsWith(".new")) throw new Error("EPERM: remove failed");
        nodeFs.remove(path);
      },
    };
    expect(await runCli(["install"], makeDeps({ fs }))).toBe(1);
    const text = err.join("\n");
    expect(text).toContain(
      `cannot copy ${join(root, "dist", "hub.exe")} to ${p.installedExe}.new: ENOSPC`,
    );
    expect(text).not.toContain("EPERM");
    expect(events).toEqual([
      "signal",
      `kill:${DAEMON_PID}`,
      "copy:hub.exe.new",
      "spawn:hubd.exe",
      "spawn:SessionHubTray.exe",
    ]);
    expect(alive.has(DAEMON_PID)).toBe(true);
    expect(alive.has(TRAY_PID)).toBe(true);
  });

  test("a late failure (autostart) restarts the installed daemon and tray, exit 1", async () => {
    platform.onSignalTrayQuit = () => {
      events.push("signal");
      trayExitAt = clock + 100;
      return alive.has(TRAY_PID);
    };
    expect(await runCli(["install"], makeDeps())).toBe(0);
    events = [];
    err = [];
    platform.onSetAutostart = () => {
      throw new Error("reg failed with exit code 1");
    };
    expect(await runCli(["install"], makeDeps())).toBe(1);
    expect(err.join("\n")).toContain("hub install: FAILED: reg failed with exit code 1");
    expect(events.slice(-2)).toEqual(["spawn:hubd.exe", "spawn:SessionHubTray.exe"]);
    expect(alive.has(DAEMON_PID)).toBe(true);
    expect(alive.has(TRAY_PID)).toBe(true);
  });

  test("a daemon that does not start still gets the tray back, exit 1", async () => {
    const deps = makeDeps({
      spawnDetached: (argv) => {
        const exe = argv[0] ?? "";
        events.push(`spawn:${basename(exe)}`);
        if (exe === paths().installedTrayExe) trayRunning();
      },
    });
    expect(await runCli(["install"], deps)).toBe(1);
    expect(err.join("\n")).toContain("the daemon did not start within 5 s");
    expect(events.slice(-2)).toEqual(["spawn:hubd.exe", "spawn:SessionHubTray.exe"]);
    expect(alive.has(TRAY_PID)).toBe(true);
  });

  test("a tray pid reused by another program during the wait is not killed", async () => {
    trayRunning();
    platform.onSignalTrayQuit = () => {
      events.push("signal");
      return true;
    };
    // after 2 s the tray exits and its pid now runs another program (no instance mutex any more)
    platform.onTrayInstanceExists = () => clock < 2_000;
    // (until the new tray is started under the same fake pid)
    platform.onProcessImagePath = (pid) =>
      pid === TRAY_PID && clock >= 2_000 && !events.includes("spawn:SessionHubTray.exe")
        ? OTHER_IMAGE
        : (images.get(pid) ?? null);
    expect(await runCli(["install"], makeDeps())).toBe(0);
    expect(killed).not.toContain(TRAY_PID);
    expect(alive.has(TRAY_PID)).toBe(true);
    expect(err).toEqual([]);
  });

  test("a daemon that does not stop restarts the tray, exit 1", async () => {
    platform.onSignalTrayQuit = () => {
      events.push("signal");
      trayExitAt = clock + 100;
      return alive.has(TRAY_PID);
    };
    expect(await runCli(["install"], makeDeps())).toBe(0);
    unkillable.add(DAEMON_PID);
    events = [];
    expect(await runCli(["install"], makeDeps())).toBe(1);
    expect(err.join("\n")).toContain(`daemon pid=${DAEMON_PID} did not stop`);
    expect(events).toEqual(["signal", `kill:${DAEMON_PID}`, "spawn:SessionHubTray.exe"]);
    expect(alive.has(TRAY_PID)).toBe(true);
  });

  test("run from the installed exe: the tray is restarted, the daemon left alone", async () => {
    const p = paths();
    mkdirSync(p.binDir, { recursive: true });
    writeFileSync(p.installedExe, "installed hub");
    writeFileSync(p.installedDaemonExe, "installed hubd");
    writeFileSync(p.installedTrayExe, "installed tray");
    daemonRunning();
    trayRunning();
    platform.onSignalTrayQuit = () => {
      events.push("signal");
      trayExitAt = clock + 100;
      return true;
    };
    const deps = makeDeps({ exePath: p.installedExe, daemonArgv: [p.installedDaemonExe] });
    expect(await runCli(["install"], deps)).toBe(0);
    expect(killed).toEqual([]);
    expect(readFileSync(p.installedTrayExe, "utf8")).toBe("installed tray");
    expect(events).toEqual([
      "signal",
      "set:SessionHubDaemon",
      "set:SessionHubTray",
      "spawn:SessionHubTray.exe",
    ]);
    expect(out.join("\n")).toContain(`ALREADY RUNNING pid=${DAEMON_PID}`);
    expect(out.join("\n")).toContain(`tray: STARTED pid=${TRAY_PID}`);
  });

  test("run from the installed exe: a stale tray.pid is not taken for a running tray", async () => {
    const p = paths();
    mkdirSync(p.binDir, { recursive: true });
    writeFileSync(p.installedExe, "installed hub");
    writeFileSync(p.installedDaemonExe, "installed hubd");
    writeFileSync(p.installedTrayExe, "installed tray");
    trayRunning(777, OTHER_IMAGE);
    const deps = makeDeps({ exePath: p.installedExe, daemonArgv: [p.installedDaemonExe] });
    expect(await runCli(["install"], deps)).toBe(0);
    expect(spawned).toEqual([[p.installedDaemonExe], [p.installedTrayExe]]);
    expect(out.join("\n")).not.toContain("tray: ALREADY RUNNING");
  });
});

describe("install checks the build manifest (INF-08c)", () => {
  const dist = () => join(root, "dist");

  test("a good install prints the commit and keeps the manifest beside the exes", async () => {
    expect(await runCli(["install"], makeDeps())).toBe(0);
    expect(out.join("\n")).toContain(`build: ${TEST_COMMIT.slice(0, 12)}`);
    expect(readFileSync(join(paths().binDir, MANIFEST_FILE), "utf8")).toBe(
      readFileSync(join(dist(), MANIFEST_FILE), "utf8"),
    );
  });

  test("a build from uncommitted changes is refused before anything is stopped", async () => {
    writeBuildManifest(dist(), true);
    expect(await runCli(["install"], makeDeps())).toBe(1);
    expect(err.join("\n")).toContain("built from uncommitted changes");
    expect(err.join("\n")).toContain("--allow-unreviewed");
    expect(platform.calls).toEqual([]);
    expect(events).toEqual([]);
  });

  test("--allow-unreviewed installs it, saying it is from uncommitted changes", async () => {
    writeBuildManifest(dist(), true);
    expect(await runCli(["install", "--allow-unreviewed"], makeDeps())).toBe(0);
    expect(out.join("\n")).toContain("(built from uncommitted changes)");
    expect(err).toEqual([]);
  });

  test("any other install argument is a usage error", async () => {
    expect(await runCli(["install", "--force"], makeDeps())).toBe(2);
    expect(events).toEqual([]);
  });

  test("no manifest: refused before the tray is signalled or anything is copied", async () => {
    rmSync(join(dist(), MANIFEST_FILE));
    expect(await runCli(["install"], makeDeps())).toBe(1);
    expect(err.join("\n")).toContain(`${MANIFEST_FILE} is missing or incomplete`);
    expect(err.join("\n")).toContain("bun run build");
    expect(platform.calls).toEqual([]);
    expect(events).toEqual([]);
  });

  test("an incomplete manifest is refused", async () => {
    const text = readFileSync(join(dist(), MANIFEST_FILE), "utf8");
    writeFileSync(
      join(dist(), MANIFEST_FILE),
      text
        .split("\n")
        .filter((l) => !l.endsWith("hubd.exe"))
        .join("\n"),
    );
    expect(await runCli(["install"], makeDeps())).toBe(1);
    expect(err.join("\n")).toContain("missing or incomplete");
    expect(events).toEqual([]);
  });

  test("an exe changed after the build is refused, naming the file", async () => {
    writeFileSync(join(dist(), "SessionHubTray.exe"), "tray rebuilt alone");
    expect(await runCli(["install"], makeDeps())).toBe(1);
    expect(err.join("\n")).toContain(
      `refusing: ${join(dist(), "SessionHubTray.exe")} does not match the build manifest`,
    );
    expect(platform.calls).toEqual([]);
    expect(events).toEqual([]);
    expect(existsSync(paths().binDir)).toBe(false);
  });

  test("a copy that does not match is rolled back and the installed build restarted", async () => {
    const p = paths();
    platform.onSignalTrayQuit = () => {
      events.push("signal");
      trayExitAt = clock + 100;
      return alive.has(TRAY_PID);
    };
    expect(await runCli(["install"], makeDeps())).toBe(0);
    writeFileSync(join(dist(), "hub.exe"), "hub v3");
    writeFileSync(join(dist(), "hubd.exe"), "hubd v3");
    writeBuildManifest(dist());
    events = [];
    err = [];
    const fs: CliFs = {
      ...recordingFs,
      copy: (src, dst) => {
        events.push(`copy:${basename(dst)}`);
        nodeFs.copy(src, dst);
        if (basename(dst) === "hubd.exe.new") writeFileSync(dst, "corrupted on the way");
      },
    };
    expect(await runCli(["install"], makeDeps({ fs }))).toBe(1);
    expect(err.join("\n")).toContain(
      `cannot copy ${join(dist(), "hubd.exe")} to ${p.installedDaemonExe}.new: the copy does not match the build manifest`,
    );
    expect(err.join("\n")).not.toContain("Close any open hub window");
    // nothing was swapped: no mix of old and new exes
    expect(readFileSync(p.installedExe, "utf8")).toBe("hub v2");
    expect(readFileSync(p.installedDaemonExe, "utf8")).toBe("hubd v2");
    expect(existsSync(`${p.installedExe}.new`)).toBe(false);
    expect(existsSync(`${p.installedDaemonExe}.new`)).toBe(false);
    expect(existsSync(join(p.binDir, MANIFEST_FILE))).toBe(true);
    expect(alive.has(DAEMON_PID)).toBe(true);
    expect(alive.has(TRAY_PID)).toBe(true);
  });

  test("a swap that fails part-way puts back the exes already swapped", async () => {
    const p = paths();
    platform.onSignalTrayQuit = () => {
      events.push("signal");
      trayExitAt = clock + 100;
      return alive.has(TRAY_PID);
    };
    expect(await runCli(["install"], makeDeps())).toBe(0);
    for (const [name, text] of [
      ["hub.exe", "hub v3"],
      ["hubd.exe", "hubd v3"],
      ["SessionHubTray.exe", "tray v3"],
    ] as const) {
      writeFileSync(join(dist(), name), text);
    }
    writeBuildManifest(dist());
    const oldManifest = readFileSync(join(p.binDir, MANIFEST_FILE), "utf8");
    renameFailures.set("hubd.exe", Number.POSITIVE_INFINITY);
    err = [];
    expect(await runCli(["install"], makeDeps())).toBe(1);
    expect(err.join("\n")).toContain(`cannot replace ${p.installedDaemonExe}`);
    expect(readFileSync(p.installedExe, "utf8")).toBe("hub v2");
    expect(readFileSync(p.installedDaemonExe, "utf8")).toBe("hubd v2");
    expect(readFileSync(p.installedTrayExe, "utf8")).toBe("tray v2");
    for (const f of [p.installedExe, p.installedDaemonExe, p.installedTrayExe]) {
      expect(existsSync(`${f}.new`)).toBe(false);
      expect(existsSync(`${f}.old`)).toBe(false);
    }
    // the old manifest went before the first swap and came back with the old exes
    expect(readFileSync(join(p.binDir, MANIFEST_FILE), "utf8")).toBe(oldManifest);
    expect(alive.has(DAEMON_PID)).toBe(true);
    expect(alive.has(TRAY_PID)).toBe(true);
  });

  test("a backup that cannot be put back is kept, and its path printed", async () => {
    const p = paths();
    expect(await runCli(["install"], makeDeps())).toBe(0);
    for (const [name, text] of [
      ["hub.exe", "hub v3"],
      ["hubd.exe", "hubd v3"],
      ["SessionHubTray.exe", "tray v3"],
    ] as const) {
      writeFileSync(join(dist(), name), text);
    }
    writeBuildManifest(dist());
    err = [];
    const fs: CliFs = {
      ...recordingFs,
      rename: (from, to) => {
        if (basename(to) === "hubd.exe") throw new Error("EBUSY: resource busy or locked");
        if (basename(from) === "hub.exe.old") throw new Error("EPERM: held by a scanner");
        nodeFs.rename(from, to);
      },
    };
    expect(await runCli(["install"], makeDeps({ fs }))).toBe(1);
    const text = err.join("\n");
    expect(text).toContain(`could not put back ${p.installedExe}: EPERM`);
    expect(text).toContain(`the old version is kept at ${p.installedExe}.old`);
    expect(readFileSync(`${p.installedExe}.old`, "utf8")).toBe("hub v2");
    expect(existsSync(`${p.installedDaemonExe}.old`)).toBe(false);
    expect(existsSync(`${p.installedDaemonExe}.new`)).toBe(false);
    // a mix is installed: no manifest may claim otherwise
    expect(existsSync(join(p.binDir, MANIFEST_FILE))).toBe(false);
  });

  test("run from the installed exe, which has no manifest: not checked", async () => {
    const p = paths();
    mkdirSync(p.binDir, { recursive: true });
    writeFileSync(p.installedExe, "installed hub");
    writeFileSync(p.installedDaemonExe, "installed hubd");
    writeFileSync(p.installedTrayExe, "installed tray");
    const deps = makeDeps({ exePath: p.installedExe, daemonArgv: [p.installedDaemonExe] });
    expect(await runCli(["install"], deps)).toBe(0);
    expect(err).toEqual([]);
  });
});

describe("uninstall with the tray app", () => {
  async function installed(): Promise<void> {
    expect(await runCli(["install"], makeDeps())).toBe(0);
    out = [];
    events = [];
    platform.calls.length = 0;
  }

  test("quits the tray, clears both Run values, stops the daemon, removes bin", async () => {
    await installed();
    platform.onSignalTrayQuit = () => {
      events.push("signal");
      trayExitAt = clock + 200;
      return true;
    };
    expect(await runCli(["uninstall"], makeDeps())).toBe(0);
    expect(events).toEqual([
      "signal",
      "clear:SessionHubDaemon",
      "clear:SessionHubTray",
      `kill:${DAEMON_PID}`,
    ]);
    expect(changes()).toEqual([
      { method: "signalTrayQuit", args: [] },
      { method: "clearAutostart", args: ["SessionHubDaemon"] },
      { method: "clearAutostart", args: ["SessionHubTray"] },
    ]);
    expect(killed).toEqual([DAEMON_PID]);
    expect(existsSync(paths().binDir)).toBe(false);
    expect(out.join("\n")).toContain("tray: STOPPED");
  });

  test("a tray that ignores the signal is killed after 5 s", async () => {
    await installed();
    expect(await runCli(["uninstall"], makeDeps())).toBe(0);
    expect(killed).toEqual([TRAY_PID, DAEMON_PID]);
    expect(clock).toBeGreaterThanOrEqual(5_000);
  });

  test("a tray that cannot be stopped fails the uninstall but the rest still runs", async () => {
    await installed();
    unkillable.add(TRAY_PID);
    expect(await runCli(["uninstall"], makeDeps())).toBe(1);
    expect(err.join("\n")).toContain(`tray pid=${TRAY_PID}`);
    expect(platform.calls).toContainEqual({ method: "clearAutostart", args: ["SessionHubTray"] });
    expect(killed).toContain(DAEMON_PID);
  });

  test("tray.pid naming another program: never killed, uninstall goes on", async () => {
    await installed();
    alive.delete(TRAY_PID);
    trayRunning(777, OTHER_IMAGE);
    expect(await runCli(["uninstall"], makeDeps())).toBe(0);
    expect(killed).toEqual([DAEMON_PID]);
    expect(out.join("\n")).toContain("tray.pid points at another program (pid=777)");
  });

  test("nothing installed: signals (a no-op), clears both Run values", async () => {
    expect(await runCli(["uninstall"], makeDeps())).toBe(0);
    expect(changes()).toEqual([
      { method: "signalTrayQuit", args: [] },
      { method: "clearAutostart", args: ["SessionHubDaemon"] },
      { method: "clearAutostart", args: ["SessionHubTray"] },
    ]);
    expect(killed).toEqual([]);
  });
});
