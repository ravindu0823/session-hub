import { STRAY_WARN_MS } from "../config";
import type { SessionView, Store } from "../store/db";
import { cleanText } from "../tui/sanitize";

export interface HubSnapshot {
  sessions: SessionView[];
  meta: {
    unknownCount: number;
    unknownVersion: string | null;
    /** Profiles whose live list could not be refreshed, with the time it went stale (epoch ms). */
    stale: { profile: string; since: number }[];
    daemon: "running" | "stopped";
    /**
     * Relay messages that landed in a session other than the one they were sent to (verify.ts),
     * while the last one is under STRAY_WARN_MS old; else 0.
     */
    relayStrayCount: number;
  };
  totals: { waitingItems: number; needsApproval: number; live: number };
}

/** Every string the panes print from a session goes through cleanText here, once per read. */
export function cleanSession(s: SessionView): SessionView {
  const st = s.status;
  return {
    ...s,
    name: cleanText(s.name),
    cwd: cleanText(s.cwd),
    profile: cleanText(s.profile),
    status:
      st === null
        ? null
        : {
            ...st,
            phase: st.phase === null ? null : cleanText(st.phase),
            remaining: st.remaining.map(cleanText),
            waiting: st.waiting.map((w) => ({
              ...w,
              id: w.id === null ? null : cleanText(w.id),
              text: cleanText(w.text),
              children: w.children.map(cleanText),
            })),
            recommendations: st.recommendations.map(cleanText),
          },
  };
}

/** Read-only snapshot of everything the TUI and the bridge show outside History and Send. */
export function readSnapshot(
  store: Store,
  opts: { daemonRunning: boolean; knownProfiles: readonly string[]; now?: number },
): HubSnapshot {
  const sessions = store.listSessions().map(cleanSession);
  const profiles = new Set<string>([...opts.knownProfiles, ...sessions.map((s) => s.profile)]);
  const stale: HubSnapshot["meta"]["stale"] = [];
  for (const profile of profiles) {
    const v = store.getMeta(`stale_since:${profile}`);
    const since = v === null ? Number.NaN : Number(v);
    if (Number.isFinite(since)) stale.push({ profile, since });
  }
  const count = Number(store.getMeta("unknown_count") ?? "0");
  const version = store.getMeta("unknown_version");
  const stray = Number(store.getMeta("relay_stray_count") ?? "0");
  const strayLast = Number(store.getMeta("relay_stray_last") ?? Number.NaN);
  const strayRecent = (opts.now ?? Date.now()) - strayLast <= STRAY_WARN_MS;
  const live = sessions.filter((s) => s.endedAt === null);
  return {
    sessions,
    meta: {
      unknownCount: Number.isFinite(count) ? count : 0,
      unknownVersion: version === null || version === "" ? null : cleanText(version),
      stale,
      daemon: opts.daemonRunning ? "running" : "stopped",
      relayStrayCount: strayRecent && Number.isSafeInteger(stray) && stray > 0 ? stray : 0,
    },
    totals: {
      waitingItems: live.reduce((n, s) => n + s.waitingCount, 0),
      needsApproval: live.filter((s) => s.state === "waiting").length,
      live: live.length,
    },
  };
}
