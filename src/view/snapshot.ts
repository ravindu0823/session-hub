import { STRAY_WARN_MS } from "../config";
import { OWNER_ROLE, parseRolesConfig, type RolesConfig } from "../mail/roles";
import { liveSessionsByRole } from "../mail/service";
import type { DecisionRow } from "../mail/store";
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
  mail: MailSummary;
}

/** A decision as the panes show it: every string cleaned, the conflicts counted. */
export interface DecisionView {
  id: string;
  role: string;
  state: DecisionRow["state"];
  question: string;
  options: string[];
  recommendation: string | null;
  answer: string | null;
  answeredAt: number | null;
  answeredVia: string | null;
  /** Answers recorded but not applied (a different second answer) or replaced. */
  conflicts: number;
  askedAt: number;
}

export interface MailSummary {
  /** Open decisions oldest first, then answered ones newest first; closed ones are left out. */
  decisions: DecisionView[];
  /** Roles with queued mail and no live session: start a session of that role to deliver it. */
  waitingRoles: { role: string; queued: number }[];
  /** Delivered mail still unacknowledged after its sender was told. */
  flagged: number;
  /** Open mail addressed to the owner. */
  ownerOpen: number;
}

/** Decisions shown at most; older answered ones stay in the index until retention. */
const MAX_DECISIONS = 100;

function decisionView(d: DecisionRow): DecisionView {
  return {
    id: cleanText(d.id),
    role: cleanText(d.role),
    state: d.state,
    question: cleanText(d.question),
    options: d.options.map(cleanText),
    recommendation: d.recommendation === null ? null : cleanText(d.recommendation),
    answer: d.answer === null ? null : cleanText(d.answer),
    answeredAt: d.answeredAt,
    answeredVia: d.answeredVia === null ? null : cleanText(d.answeredVia),
    conflicts: d.conflicts.length,
    askedAt: d.createdAt,
  };
}

/** Mail and decisions for the panes. */
export function readMailSummary(store: Store, roles: RolesConfig): MailSummary {
  const mail = store.mail();
  const answered = mail
    .listDecisions({ state: "answered" })
    .sort((a, b) => (b.answeredAt ?? 0) - (a.answeredAt ?? 0) || b.seq - a.seq);
  const decisions = [...mail.listDecisions({ state: "open" }), ...answered]
    .slice(0, MAX_DECISIONS)
    .map(decisionView);
  const live = liveSessionsByRole(store, roles);
  const waitingRoles = [...mail.queuedByRole()]
    .filter(([role]) => role !== OWNER_ROLE && !live.has(role))
    .map(([role, queued]) => ({ role: cleanText(role), queued }));
  return {
    decisions,
    waitingRoles,
    flagged: mail.countFlagged(),
    ownerOpen: mail.countOpenTo(OWNER_ROLE),
  };
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
  opts: {
    daemonRunning: boolean;
    knownProfiles: readonly string[];
    now?: number;
    /** The local roles file, parsed; without it a session's role is its name. */
    roles?: RolesConfig;
  },
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
    mail: readMailSummary(store, opts.roles ?? parseRolesConfig(null)),
  };
}
