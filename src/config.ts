import { homedir } from "node:os";
import { join } from "node:path";

export type ProfileId = "claude" | "claude-personal";
export type Profile = { id: ProfileId; dir: string };
export type Config = { profiles: readonly Profile[]; dataDir: string };

export const DEBOUNCE_MS = 500;
export const SAFETY_POLL_MS = 30_000;
export const AGENTS_TIMEOUT_MS = 10_000;
export const RETENTION_MS = 7 * 86_400_000;
export const FIRST_INDEX_BYTES = 20 * 1024 * 1024;
export const RELAY_TIMEOUT_MS = 120_000;
export const DELIVERY_WINDOW_MS = 30 * 60_000;
/** The daemon expires overdue sends this often. */
export const EXPIRE_EVERY_MS = 30_000;
export const RELAY_NAME = "session-hub-relay";
/** Name prefix of the throwaway sessions scripts/relay-eval.ts starts: relays to them are expected. */
export const EVAL_SESSION_PREFIX = "sh-eval-";
/** How long after the last stray relay message the TUI and the tray keep warning about it. */
export const STRAY_WARN_MS = 24 * 3600_000;
/** Per-relay trace (outcome, timing, cost, tokens, model; never text), in the run folder. */
export const RELAY_LOG = "relay.log";
/**
 * In the run folder: the daemon was stopped on purpose (`hub daemon stop`, install, uninstall), so
 * the tray's bridge does not start it again. Any daemon start, and every daemon run, removes it.
 */
export const DAEMON_STOPPED_FILE = "daemon.stopped";

export const MESSAGE_HEADER =
  "[Session Hub: message from the owner. Reply in your normal terminal reply; do not SendMessage back, the sender has exited.]";

/** Pure: paths come from the injected env so tests never touch real folders. `.tclaude` is never listed. */
export function resolveConfig(env: Record<string, string | undefined>): Config {
  const userProfile = env.USERPROFILE;
  const localAppData = env.LOCALAPPDATA;
  if (!userProfile) throw new Error("USERPROFILE is not set");
  if (!localAppData) throw new Error("LOCALAPPDATA is not set");
  return {
    profiles: [
      { id: "claude", dir: join(userProfile, ".claude") },
      { id: "claude-personal", dir: join(userProfile, ".claude-personal") },
    ],
    dataDir: join(localAppData, "SessionHub"),
  };
}

const live = resolveConfig({
  USERPROFILE: process.env.USERPROFILE ?? homedir(),
  LOCALAPPDATA: process.env.LOCALAPPDATA ?? join(homedir(), "AppData", "Local"),
});

export const PROFILES: readonly Profile[] = live.profiles;
export const DATA_DIR: string = live.dataDir;
