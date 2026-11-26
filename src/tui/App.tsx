import { Box, type Key, Text, useApp, useInput, usePaste, useWindowSize } from "ink";
import { type ReactNode, useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { ProfileId } from "../config";
import type { RolesConfig } from "../mail/roles";
import type { SessionView, Store } from "../store/db";
import { type MailSummary, readSnapshot } from "../view/snapshot";
import { DecisionsPane } from "./DecisionsPane";
import { sessionLabel } from "./format";
import { HistoryPane } from "./HistoryPane";
import type { PaneKeys, Register } from "./keys";
import { type SendFn, SendPane } from "./SendPane";
import { groupSessions, SessionList } from "./SessionList";
import { type HubMeta, StatusPane } from "./StatusPane";
import { cleanText } from "./sanitize";

export interface AppProps {
  store: Store;
  send: SendFn;
  focus: (pid: number) => Promise<{ ok: boolean; title?: string; reason?: string }>;
  buildMessage: (text: string) => string;
  now: () => number;
  refreshMs?: number;
  /** The local roles file, parsed; read on every refresh (default: none, a session's role is its name). */
  roles?: () => RolesConfig;
}

type Tab = "status" | "history" | "send" | "decisions";

const KNOWN_PROFILES: readonly ProfileId[] = ["claude", "claude-personal"];
const TOAST_MS = 3000;

interface Snapshot {
  sessions: SessionView[];
  meta: HubMeta;
  mail: MailSummary;
}

function message(e: unknown): string {
  return cleanText(e instanceof Error ? e.message : String(e));
}

export function App(props: AppProps) {
  const { store, focus, roles } = props;
  const refreshMs = props.refreshMs ?? 1000;
  const { exit } = useApp();
  const { columns, rows } = useWindowSize();
  const [snap, setSnap] = useState<Snapshot | null>(null);
  const [storeError, setStoreError] = useState<string | null>(null);
  const [paneError, setPaneError] = useState<string | null>(null);
  const [tick, setTick] = useState(0);
  const [toast, setToast] = useState<string | null>(null);
  const [searchNonce, setSearchNonce] = useState(0);
  const toastTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const mounted = useRef(true);

  // Key routing reads refs, never render-time state: keys that arrive in one tick (fast typing,
  // a paste, a terminal burst) must each see what the key before them did.
  const [tab, setTabState] = useState<Tab>("status");
  const tabRef = useRef<Tab>("status");
  const setTab = (t: Tab) => {
    tabRef.current = t;
    setTabState(t);
  };
  const [selectedId, setSelectedState] = useState<string | null>(null);
  const selectedRef = useRef<string | null>(null);
  const setSelected = (id: string | null) => {
    selectedRef.current = id;
    setSelectedState(id);
  };
  const [quitPrompt, setQuitState] = useState(false);
  const quitRef = useRef(false);
  const setQuit = (b: boolean) => {
    quitRef.current = b;
    setQuitState(b);
  };
  const panes = useRef<Partial<Record<Tab, PaneKeys>>>({});
  const drafts = useRef(new Map<string, string>()).current;
  const register = useMemo(() => {
    const make =
      (t: Tab): Register =>
      (keys) => {
        if (keys === null) delete panes.current[t];
        else panes.current[t] = keys;
      };
    return { status: make("status"), history: make("history"), send: make("send") };
  }, []);

  const refresh = useCallback(() => {
    try {
      setSnap(
        readSnapshot(store, {
          daemonRunning: true,
          knownProfiles: KNOWN_PROFILES,
          ...(roles === undefined ? {} : { roles: roles() }),
        }),
      );
      setStoreError(null);
      setPaneError(null);
    } catch (e) {
      setStoreError(message(e));
    }
    setTick((t) => t + 1);
  }, [store, roles]);

  // Reads run in timers, never during render; the first frame shows "loading".
  useEffect(() => {
    const first = setTimeout(refresh, 0);
    const every = setInterval(refresh, refreshMs);
    return () => {
      clearTimeout(first);
      clearInterval(every);
    };
  }, [refresh, refreshMs]);

  useEffect(
    () => () => {
      mounted.current = false;
      if (toastTimer.current) clearTimeout(toastTimer.current);
    },
    [],
  );

  const showToast = useCallback((text: string) => {
    if (!mounted.current) return;
    if (toastTimer.current) clearTimeout(toastTimer.current);
    setToast(cleanText(text));
    toastTimer.current = setTimeout(() => {
      if (mounted.current) setToast(null);
    }, TOAST_MS);
  }, []);

  const onPaneError = useCallback((m: string) => setPaneError(cleanText(m)), []);

  const groups = groupSessions(snap?.sessions ?? []);
  const flat = groups.flatMap((g) => g.sessions);
  const resolve = (id: string | null) => flat.find((s) => s.sessionId === id) ?? flat[0] ?? null;
  const selected = resolve(selectedId);

  /** The active pane's keys, only if they belong to the session selected right now. */
  const livePane = (): PaneKeys | null => {
    const sel = resolve(selectedRef.current);
    const pane = panes.current[tabRef.current];
    return pane && sel && pane.sessionId === sel.sessionId ? pane : null;
  };

  const onKey = (input: string, key: Key) => {
    if (quitRef.current) {
      if (input === "y") exit();
      else if (input === "n" || key.escape) setQuit(false);
      return;
    }
    const pane = livePane();
    if (pane?.captures()) {
      pane.handle(input, key);
      return;
    }
    const sel = resolve(selectedRef.current);
    if (input === "q") {
      const unsent = [...drafts.values()].some((d) => d.trim() !== "");
      if (unsent) setQuit(true);
      else exit();
    } else if (input === "r") refresh();
    else if (input === "1") setTab("status");
    else if (input === "2") setTab("history");
    else if (input === "3") setTab("send");
    else if (input === "4") setTab("decisions");
    else if (input === "/") {
      setTab("history");
      const hp = panes.current.history;
      if (hp && sel && hp.sessionId === sel.sessionId && hp.openSearch) hp.openSearch();
      else setSearchNonce((n) => n + 1);
    } else if (key.upArrow || key.downArrow) {
      if (flat.length === 0) return;
      const i = sel ? flat.indexOf(sel) : 0;
      const next = key.upArrow ? Math.max(0, i - 1) : Math.min(flat.length - 1, i + 1);
      setSelected(flat[next]?.sessionId ?? null);
    } else if (input === "j" && sel) {
      if (sel.endedAt !== null) {
        showToast("could not focus: session ended");
        return;
      }
      focus(sel.pid).then(
        (r) =>
          showToast(
            r.ok
              ? `focused: ${r.title ?? "window"}`
              : `could not focus: ${r.reason ?? "unknown reason"}`,
          ),
        (e: unknown) => showToast(`could not focus: ${message(e)}`),
      );
    } else {
      pane?.handle(input, key);
    }
  };
  useInput(onKey);
  usePaste((text) => {
    if (quitRef.current) return;
    const pane = livePane();
    if (pane?.captures()) pane.paste?.(text);
  });

  const height = Math.max(6, rows - 1);
  const bodyH = height - 2;
  const leftW = Math.max(22, Math.floor(columns * 0.3));
  const rightW = Math.max(10, columns - leftW - 1);
  const error = storeError ?? paneError;

  const openDecisions = snap?.mail.decisions.filter((d) => d.state === "open").length ?? 0;
  const decisionsLabel = openDecisions > 0 ? `Decisions (${openDecisions})` : "Decisions";
  const tabLabel = (t: Tab, n: string, label: string) => (
    <Text inverse={tab === t} bold={tab === t}>{` ${n} ${label} `}</Text>
  );

  const decisionsBox =
    snap === null ? null : (
      <Box display={tab === "decisions" ? "flex" : "none"}>
        <DecisionsPane mail={snap.mail} width={rightW} height={bodyH} />
      </Box>
    );
  let pane: ReactNode;
  if (snap === null) {
    pane = <Text dimColor>{storeError ? "store unavailable" : "loading..."}</Text>;
  } else if (selected === null) {
    pane = tab === "decisions" ? decisionsBox : <Text dimColor>no sessions yet</Text>;
  } else {
    const key = selected.sessionId;
    pane = (
      <>
        <Box display={tab === "status" ? "flex" : "none"}>
          <StatusPane
            key={`st:${key}`}
            session={selected}
            meta={snap.meta}
            now={props.now()}
            width={rightW}
            height={bodyH}
            register={register.status}
          />
        </Box>
        <Box display={tab === "history" ? "flex" : "none"}>
          <HistoryPane
            key={`hi:${key}`}
            store={store}
            session={selected}
            sessions={snap.sessions}
            tick={tick}
            width={rightW}
            height={bodyH}
            active={tab === "history"}
            searchNonce={searchNonce}
            register={register.history}
            onError={onPaneError}
          />
        </Box>
        <Box display={tab === "send" ? "flex" : "none"}>
          <SendPane
            key={`se:${key}`}
            store={store}
            session={selected}
            tick={tick}
            send={props.send}
            buildMessage={props.buildMessage}
            width={rightW}
            height={bodyH}
            drafts={drafts}
            register={register.send}
            onError={onPaneError}
          />
        </Box>
        {decisionsBox}
      </>
    );
  }

  let footer: ReactNode;
  if (quitPrompt) {
    footer = (
      <Text color="yellow" bold wrap="truncate-end">
        quit and discard draft? y/n
      </Text>
    );
  } else if (error) {
    footer = <Text color="red" wrap="truncate-end">{`store error: ${error}`}</Text>;
  } else if (toast) {
    footer = (
      <Text color="yellow" wrap="truncate-end">
        {toast}
      </Text>
    );
  } else {
    footer = (
      <Text dimColor wrap="truncate-end">
        ↑↓ select · 1-4 pane · / search · j jump · r refresh · q quit · times local
      </Text>
    );
  }

  return (
    <Box flexDirection="column" width={columns} height={height}>
      <Box width={columns}>
        <Text wrap="truncate-end">
          <Text bold>Session Hub </Text>
          {tabLabel("status", "1", "Status")}
          {tabLabel("history", "2", "History")}
          {tabLabel("send", "3", "Send")}
          {tabLabel("decisions", "4", decisionsLabel)}
          <Text dimColor>{selected ? `  ${sessionLabel(selected)}` : ""}</Text>
        </Text>
      </Box>
      <Box height={bodyH} width={columns}>
        <Box
          width={leftW}
          flexShrink={0}
          borderStyle="single"
          borderTop={false}
          borderBottom={false}
          borderLeft={false}
        >
          <SessionList
            groups={groups}
            selectedId={selected?.sessionId ?? null}
            width={leftW - 1}
            height={bodyH}
            emptyText={snap ? "no sessions" : ""}
          />
        </Box>
        <Box paddingLeft={1} width={rightW + 1} flexDirection="column" overflow="hidden">
          {pane}
        </Box>
      </Box>
      <Box width={columns}>{footer}</Box>
    </Box>
  );
}
