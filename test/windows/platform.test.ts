import { afterAll, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createWindowsPlatform } from "../../src/platform/windows";
import { setAutostart } from "../../src/platform/windows/autostart";
import { sysbin } from "../../src/platform/windows/sysbin";

const win = process.platform === "win32";
// Writes HKCU Run values and focuses windows: only through `bun run test:windows`, never plain `bun test`.
const runWin = win && process.env.SESSION_HUB_WINDOWS_SUITE === "1";
// Throwaway Run value names: the hub's real SessionHubDaemon and SessionHubTray are never touched.
const daemonValue = `SessionHubTest-${process.pid}-${Date.now()}-daemon`;
const trayValue = `SessionHubTest-${process.pid}-${Date.now()}-tray`;
const runValueNames = { SessionHubDaemon: daemonValue, SessionHubTray: trayValue };
const platform = createWindowsPlatform({ runValueNames });
const dirs: string[] = [];

function tempDir(): string {
  const d = mkdtempSync(join(tmpdir(), "sh-acl-"));
  dirs.push(d);
  return d;
}

async function icacls(...args: string[]): Promise<number> {
  const p = Bun.spawn([sysbin.icacls(), ...args], {
    stdout: "ignore",
    stderr: "ignore",
    windowsHide: true,
  });
  return await p.exited;
}

afterAll(async () => {
  if (runWin) {
    try {
      await platform.clearAutostart("SessionHubDaemon");
      await platform.clearAutostart("SessionHubTray");
    } catch {}
  }
  for (const d of dirs) rmSync(d, { recursive: true, force: true });
});

describe("windows acl", () => {
  test.skipIf(!runWin)("ensurePrivateDir makes the dir private", async () => {
    const d = join(tempDir(), "data");
    await platform.ensurePrivateDir(d);
    expect(await platform.isPrivateDir(d)).toBe(true);
  });

  test.skipIf(!runWin)("extra Users grant is detected", async () => {
    const d = tempDir();
    await platform.ensurePrivateDir(d);
    expect(await icacls(d, "/grant", "*S-1-5-32-545:(OI)(CI)R")).toBe(0);
    expect(await platform.isPrivateDir(d)).toBe(false);
  });

  test.skipIf(!runWin)("an explicit grant to another principal is removed", async () => {
    // An elevated admin's new folders can carry an explicit Administrators entry, which
    // /inheritance:r does not remove; Users stands in for it here.
    const d = join(tempDir(), "explicit");
    mkdirSync(d);
    expect(await icacls(d, "/grant", "*S-1-5-32-545:(OI)(CI)R")).toBe(0);
    await platform.ensurePrivateDir(d);
    expect(await platform.isPrivateDir(d)).toBe(true);
  });

  test.skipIf(!runWin)("default inherited temp dir is not private", async () => {
    const d = tempDir();
    expect(await platform.isPrivateDir(d)).toBe(false);
  });

  test.skipIf(!runWin)("missing dir is not private", async () => {
    expect(await platform.isPrivateDir(join(tempDir(), "nope"))).toBe(false);
  });

  test.skipIf(!runWin)("path with spaces works", async () => {
    const d = join(tempDir(), "a b c");
    mkdirSync(d);
    await platform.ensurePrivateDir(d);
    expect(await platform.isPrivateDir(d)).toBe(true);
  });
});

describe("windows autostart", () => {
  test.skipIf(!runWin)("set, get, clear", async () => {
    const exe = "C:\\Program Files\\Session Hub\\hubd.exe";
    await platform.setAutostart("SessionHubDaemon", exe);
    expect(await createWindowsPlatform({ runValueNames }).getAutostart("SessionHubDaemon")).toBe(
      `"${exe}"`,
    );
    await setAutostart(daemonValue, exe, "daemon run");
    expect(await platform.getAutostart("SessionHubDaemon")).toBe(`"${exe}" daemon run`);
    await platform.clearAutostart("SessionHubDaemon");
    expect(await platform.getAutostart("SessionHubDaemon")).toBeNull();
    await platform.clearAutostart("SessionHubDaemon");
  });

  test.skipIf(!runWin)("the two Run values are independent", async () => {
    const hubd = "C:\\Program Files\\Session Hub\\hubd.exe";
    const tray = "C:\\Program Files\\Session Hub\\SessionHubTray.exe";
    await platform.setAutostart("SessionHubDaemon", hubd);
    await platform.setAutostart("SessionHubTray", tray);
    expect(await platform.getAutostart("SessionHubDaemon")).toBe(`"${hubd}"`);
    expect(await platform.getAutostart("SessionHubTray")).toBe(`"${tray}"`);
    await platform.clearAutostart("SessionHubTray");
    expect(await platform.getAutostart("SessionHubTray")).toBeNull();
    expect(await platform.getAutostart("SessionHubDaemon")).toBe(`"${hubd}"`);
    await platform.clearAutostart("SessionHubDaemon");
    expect(await platform.getAutostart("SessionHubDaemon")).toBeNull();
  });
});

describe("windows focus", () => {
  test.skipIf(!runWin)(
    "own pid returns a well-formed result",
    async () => {
      const r = await platform.focusSessionWindow(process.pid);
      expect(typeof r.ok).toBe("boolean");
      if (r.ok) expect(typeof r.title).toBe("string");
      // Either outcome proves the parent walk found a window.
      if (process.env.SESSION_HUB_HEADLESS !== "1") {
        expect(r.ok === true || r.reason === "focus-denied").toBe(true);
      } else if (!r.ok) {
        expect(["no-window", "focus-denied"]).toContain(r.reason as string);
      }
    },
    20_000,
  ); // the CIM query can take more than 5 s on a cold machine

  test.skipIf(!runWin)("invalid pid is an error", async () => {
    for (const bad of [0, -1, 1.5, Number.NaN]) {
      expect(await platform.focusSessionWindow(bad)).toEqual({ ok: false, reason: "error" });
    }
  });
});

describe("non-windows behaviour", () => {
  test.skipIf(win)("methods throw, focus reports not-windows", async () => {
    await expect(platform.ensurePrivateDir("x")).rejects.toThrow("windows only");
    expect(await platform.focusSessionWindow(1)).toEqual({ ok: false, reason: "not-windows" });
  });
});
