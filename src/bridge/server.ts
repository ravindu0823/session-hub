import { createHash } from "node:crypto";
import { DELIVERY_WINDOW_MS, EXPIRE_EVERY_MS, PROFILES } from "../config.ts";
import { maskSecrets } from "../security/mask.ts";
import { refusalText } from "../send/refusals.ts";
import { SendRefused, type SendRequest } from "../send/relay.ts";
import type { MessageRow, SendRow, SendState, Store } from "../store/db.ts";
import { cleanText } from "../tui/sanitize.ts";
import { type HubSnapshot, readSnapshot } from "../view/snapshot.ts";
import {
  type BridgeEvent,
  type BridgeSnapshot,
  encode,
  HEARTBEAT_MS,
  MAX_LINE_BYTES,
  MIN_EMIT_GAP_MS,
  type ParseErrorCode,
  POLL_MS,
  PROTOCOL,
  parseRequest,
  type ReplyError,
  type Request,
} from "./protocol.ts";

/**
 * A send this bridge started is watched for state changes this long at most: the delivery window
 * plus two of the daemon's expiry ticks, so the daemon's `expired` still reaches the tray.
 */
export const SEND_TRACK_MS = DELIVERY_WINDOW_MS + 2 * EXPIRE_EVERY_MS;
/** A history message longer than this (UTF-8 bytes) is cut and flagged `textTruncated`. */
export const MAX_MESSAGE_BYTES = 256 * 1024;
/**
 * A daemon that was running and stopped without `hub daemon stop` (a crash) is started again at
 * once, then after these delays while it keeps failing to start or keeps dying; after the last one
 * the bridge gives up and reports daemon-restart-failed (RES-L1).
 */
export const RESTART_DELAYS_MS: readonly number[] = [5_000, 30_000, 120_000];
/** Uptime after which a daemon counts as healthy again and the backoff starts over. */
export const RESTART_STABLE_MS = 120_000;

export interface BridgeIo {
  lines: AsyncIterable<string>;
  /** One protocol line, without the terminator. */
  write(line: string): void;
  /** Resolves with an exit code when the output is broken; the bridge then stops at once. */
  stop?: Promise<number>;
}

export interface BridgeDeps {
  /** The open index, or null while hub.db does not exist. May throw (locked, corrupt): retried. */
  store: () => Store | null;
  daemonRunning(): boolean;
  /** True when the daemon is running afterwards. */
  startDaemon(): Promise<boolean>;
  /** True when the owner stopped the daemon on purpose (`hub daemon stop`): no automatic restart. */
  daemonStopRequested?(): boolean;
  /** send() from src/send/relay.ts; onCreated fires once, right after the row is stored. */
  send(req: SendRequest, onCreated: (row: SendRow) => void): Promise<SendRow>;
  now(): number;
  sleep(ms: number): Promise<void>;
  version: string;
}

const KNOWN_PROFILES: readonly string[] = PROFILES.map((p) => p.id);

const PARSE_MESSAGES: Record<ParseErrorCode, string> = {
  "bad-json": "line is not a JSON object",
  "too-long": `line is longer than ${MAX_LINE_BYTES} bytes`,
  "unknown-type": "unknown request type",
  "bad-request": "request is missing a field or has a wrong field type",
};

const NO_STORE = "the index does not exist yet; start the daemon";

function errText(e: unknown): string {
  return cleanText(e instanceof Error ? e.message : String(e));
}

function emptySnapshot(daemonRunning: boolean): HubSnapshot {
  return {
    sessions: [],
    meta: {
      unknownCount: 0,
      unknownVersion: null,
      stale: [],
      daemon: daemonRunning ? "running" : "stopped",
      relayStrayCount: 0,
    },
    totals: { waitingItems: 0, needsApproval: 0, live: 0 },
  };
}

export interface HistoryMessage extends MessageRow {
  /** Present (true) only when `text` was cut to MAX_MESSAGE_BYTES. */
  textTruncated?: true;
}

/** The longest prefix of `s` that is at most `max` UTF-8 bytes, never splitting a character. */
function cutUtf8(s: string, max: number): string {
  const buf = Buffer.from(s, "utf8");
  if (buf.length <= max) return s;
  let end = max;
  // Step back over continuation bytes (10xxxxxx) to the start of the character that would split.
  while (end > 0 && ((buf[end] as number) & 0xc0) === 0x80) end -= 1;
  return buf.subarray(0, end).toString("utf8");
}

/** Store text is masked at ingest; masking again is idempotent and covers rows written earlier. */
function cleanMessage(m: MessageRow): HistoryMessage {
  const text = cleanText(maskSecrets(m.text));
  const cut = cutUtf8(text, MAX_MESSAGE_BYTES);
  const out: HistoryMessage = {
    ...m,
    text: cut,
    fromName: m.fromName === null ? null : cleanText(maskSecrets(m.fromName)),
  };
  if (cut.length !== text.length) out.textTruncated = true;
  return out;
}

const fits = (line: string): boolean => Buffer.byteLength(line, "utf8") <= MAX_LINE_BYTES;

/**
 * The snapshot as the tray gets it: never a transcript path. `reduced` also drops the status of
 * ended sessions, for a snapshot that does not fit in one line otherwise.
 */
export function toBridgeSnapshot(s: HubSnapshot, reduced = false): BridgeSnapshot {
  return {
    ...s,
    sessions: s.sessions.map(({ transcriptPath: _, ...v }) =>
      reduced && v.endedAt !== null ? { ...v, status: null } : v,
    ),
  };
}

interface Tracked {
  sessionId: string;
  state: SendState;
  detail: string | null;
  since: number;
}

class Bridge {
  private seq = 0;
  private lastHash: string | null = null;
  private lastEmitAt = Number.NEGATIVE_INFINITY;
  /** Hash and time of the last snapshot that did not fit in one line. */
  private oversizeHash: string | null = null;
  private oversizeAt = Number.NEGATIVE_INFINITY;
  private dirty = true;
  private lastStore: Store | null = null;
  private lastVersion: number | null = null;
  private lastDaemon = false;
  /** When the next automatic restart may run; null when none is due. */
  private restartAt: number | null = null;
  private restartTries = 0;
  private restarting = false;
  /** When the daemon was last seen to start running. */
  private runningSince = 0;
  private lastStoreError: string | null = null;
  private malformed = 0;
  private stopped = false;
  private readonly tracked = new Map<number, Tracked>();

  constructor(
    private readonly io: BridgeIo,
    private readonly deps: BridgeDeps,
  ) {}

  async run(): Promise<number> {
    this.emit({ type: "hello", protocol: PROTOCOL, version: this.deps.version });
    this.tick();
    if (!this.lastDaemon) void this.autoStartDaemon();
    this.pollLoop().catch((e: unknown) => this.safe(() => this.error("bridge-error", errText(e))));
    const input = (async (): Promise<number> => {
      for await (const line of this.io.lines) {
        if (this.stopped || this.handleLine(line) === "shutdown") break;
      }
      return 0;
    })();
    const code = await (this.io.stop === undefined ? input : Promise.race([input, this.io.stop]));
    this.stopped = true;
    return code;
  }

  /** Runs fn; a throw here has nowhere left to go (the output itself may be broken). */
  private safe(fn: () => void): void {
    try {
      fn();
    } catch {
      // nothing to report it to
    }
  }

  /** Every request handler that runs on its own ends in exactly one reply, even if it throws. */
  private launch(id: number, work: Promise<void>): void {
    work.catch((e: unknown) =>
      this.safe(() => this.replyError(id, { code: "internal", message: errText(e) })),
    );
  }

  // ---- output ----

  /** False when the line would be over the limit (nothing is written) or the bridge stopped. */
  private emit(e: BridgeEvent): boolean {
    if (this.stopped) return false;
    const line = encode(e);
    if (!fits(line)) return false;
    this.io.write(line);
    return true;
  }

  private error(code: string, message: string): void {
    this.emit({ type: "error", code, message });
  }

  private reply(id: number, result: unknown): void {
    if (!this.emit({ type: "reply", id, ok: true, result })) {
      this.replyError(id, { code: "too-large", message: "the reply does not fit in one line" });
    }
  }

  private replyError(id: number, error: ReplyError): void {
    this.emit({ type: "reply", id, ok: false, error });
  }

  // ---- polling ----

  private async pollLoop(): Promise<void> {
    while (!this.stopped) {
      await this.deps.sleep(POLL_MS);
      if (this.stopped) return;
      try {
        this.tick();
      } catch (e) {
        this.error("bridge-error", errText(e));
      }
    }
  }

  private async autoStartDaemon(): Promise<void> {
    try {
      if (!(await this.deps.startDaemon())) {
        this.error("daemon-start-failed", "the daemon did not start; see daemon.log");
      }
    } catch (e) {
      this.error("daemon-start-failed", errText(e));
    }
  }

  /** One automatic restart attempt; schedules the next one with backoff while starts fail. */
  private async restartDaemon(): Promise<void> {
    this.restarting = true;
    this.restartTries += 1;
    let ok = false;
    try {
      ok = await this.deps.startDaemon();
    } catch {
      ok = false;
    }
    this.restarting = false;
    // A start that works does not reset the backoff: only RESTART_STABLE_MS of uptime does, so a
    // daemon that starts and dies again is not restarted in a tight loop.
    if (ok) this.restartAt = null;
    else this.scheduleRestart(this.deps.now());
  }

  /** The next restart after `restartTries` attempts: at once, then RESTART_DELAYS_MS, then give up. */
  private scheduleRestart(now: number): void {
    const delay = this.restartTries === 0 ? 0 : RESTART_DELAYS_MS[this.restartTries - 1];
    if (delay === undefined) {
      this.restartAt = null;
      this.error(
        "daemon-restart-failed",
        `the daemon stopped and ${this.restartTries} restarts did not keep it running; see daemon.log`,
      );
      return;
    }
    this.restartAt = now + delay;
  }

  private storeError(e: unknown): void {
    const message = errText(e);
    if (message === this.lastStoreError) return;
    this.lastStoreError = message;
    this.error("store-error", message);
  }

  private openStore(): Store | null {
    try {
      return this.deps.store();
    } catch (e) {
      this.storeError(e);
      return null;
    }
  }

  private tick(): void {
    const now = this.deps.now();
    const store = this.openStore();
    if (store !== this.lastStore) {
      this.lastStore = store;
      this.lastVersion = null;
      this.dirty = true;
    }
    if (store !== null) {
      try {
        const v = store.dataVersion();
        if (v !== this.lastVersion) {
          this.lastVersion = v;
          this.dirty = true;
          this.checkSends(store);
        }
      } catch (e) {
        this.storeError(e);
      }
    }
    const daemon = this.deps.daemonRunning();
    if (daemon !== this.lastDaemon) {
      if (daemon) {
        this.restartAt = null;
        this.runningSince = now;
      } else if (!(this.deps.daemonStopRequested?.() ?? false)) {
        this.scheduleRestart(now);
      }
      this.lastDaemon = daemon;
      this.dirty = true;
    }
    if (daemon && this.restartTries > 0 && now - this.runningSince >= RESTART_STABLE_MS) {
      this.restartTries = 0;
    }
    if (!daemon && !this.restarting && this.restartAt !== null && now >= this.restartAt) {
      void this.restartDaemon();
    }
    const heartbeat = now - Math.max(this.lastEmitAt, this.oversizeAt) >= HEARTBEAT_MS;
    if (now - this.lastEmitAt < MIN_EMIT_GAP_MS) return;
    if (this.dirty) {
      this.dirty = false;
      this.emitSnapshot(store, daemon, heartbeat, now);
    } else if (heartbeat) {
      // Nothing changed since the last snapshot: a small line keeps the tray's watchdog quiet.
      this.emitHeartbeat(now);
    }
  }

  private emitHeartbeat(now: number): void {
    if (this.emit({ type: "heartbeat", seq: this.seq })) this.lastEmitAt = now;
  }

  /**
   * Emits the snapshot when it differs from the last one sent. With `heartbeat` (15 s without a line)
   * a snapshot that cannot or need not be sent is replaced by a heartbeat line.
   */
  private emitSnapshot(
    store: Store | null,
    daemon: boolean,
    heartbeat: boolean,
    now: number,
  ): void {
    let snapshot: HubSnapshot;
    if (store === null) {
      snapshot = emptySnapshot(daemon);
    } else {
      try {
        snapshot = readSnapshot(store, {
          daemonRunning: daemon,
          knownProfiles: KNOWN_PROFILES,
          now,
        });
        this.lastStoreError = null;
      } catch (e) {
        this.storeError(e);
        this.dirty = true;
        if (heartbeat) this.emitHeartbeat(now);
        return;
      }
    }
    const hash = createHash("sha256").update(JSON.stringify(snapshot)).digest("hex");
    // Unchanged, or a snapshot that did not fit (tried again only once the data changes).
    if (hash === this.lastHash || hash === this.oversizeHash) {
      if (heartbeat) this.emitHeartbeat(now);
      return;
    }
    const seq = this.seq + 1;
    // Over 1 MiB (about 13x the largest real index): ended sessions go without their status.
    const sent =
      this.emit({ type: "snapshot", seq, snapshot: toBridgeSnapshot(snapshot) }) ||
      this.emit({ type: "snapshot", seq, snapshot: toBridgeSnapshot(snapshot, true) });
    if (!sent) {
      if (this.stopped) return;
      this.oversizeHash = hash;
      this.oversizeAt = now;
      this.error("too-large", "the snapshot does not fit in one line");
      if (heartbeat) this.emitHeartbeat(now);
      return;
    }
    this.seq += 1;
    this.lastHash = hash;
    this.lastEmitAt = now;
    this.oversizeHash = null;
  }

  /** Emits a send event for every tracked send whose state or detail changed. */
  private checkSends(store: Store): void {
    const now = this.deps.now();
    for (const [id, t] of this.tracked) {
      const row = store.getSend(id);
      if (row === null) {
        this.tracked.delete(id);
        continue;
      }
      if (row.state !== t.state || row.detail !== t.detail) {
        t.state = row.state;
        t.detail = row.detail;
        this.emit({
          type: "send",
          send: {
            id,
            sessionId: t.sessionId,
            state: row.state,
            detail: row.detail === null ? null : cleanText(row.detail),
          },
        });
      }
      // The row is read before tracking ends, so a change that lands right at the limit is sent.
      const final = row.state === "delivered" || row.state === "expired";
      if (final || now - t.since > SEND_TRACK_MS) this.tracked.delete(id);
    }
  }

  // ---- requests ----

  private handleLine(line: string): "shutdown" | undefined {
    if (line.trim() === "") return undefined;
    const req = parseRequest(line);
    if ("error" in req) {
      this.malformed += 1;
      const message = PARSE_MESSAGES[req.error];
      if (req.id !== undefined) this.replyError(req.id, { code: req.error, message });
      else this.error(req.error, `${message}; skipped (${this.malformed} malformed lines so far)`);
      return undefined;
    }
    if (req.type === "shutdown") return "shutdown";
    try {
      this.handle(req);
    } catch (e) {
      this.replyError(req.id, { code: "store-error", message: errText(e) });
    }
    return undefined;
  }

  private handle(req: Exclude<Request, { type: "shutdown" }>): void {
    switch (req.type) {
      case "ping":
        this.reply(req.id, {});
        return;
      case "startDaemon":
        this.launch(req.id, this.handleStartDaemon(req.id));
        return;
      case "send":
        this.launch(req.id, this.handleSend(req));
        return;
      case "history":
      case "search": {
        const store = this.openStore();
        if (store === null) {
          this.replyError(req.id, { code: "no-store", message: NO_STORE });
          return;
        }
        if (req.type === "history") this.history(store, req);
        else {
          const hits = store.search(req.q, req.limit).map((h) => ({
            ...h,
            snippet: cleanText(maskSecrets(h.snippet)),
          }));
          this.reply(req.id, { hits });
        }
        return;
      }
    }
  }

  /**
   * One page, oldest first. `more`: older messages exist beyond it. A page too big for one line
   * drops its oldest messages until it fits (`truncated`); "Load older" with the oldest ts shown
   * gets them. No single message is too big: each is cut to MAX_MESSAGE_BYTES first.
   */
  private history(store: Store, req: Extract<Request, { type: "history" }>): void {
    const rows = store.history(req.sessionId, req.limit + 1, req.beforeTs, req.beforeUuid);
    let more = rows.length > req.limit;
    let messages = (more ? rows.slice(1) : rows).map(cleanMessage);
    let truncated = false;
    for (;;) {
      const result = { messages, truncated, more };
      if (fits(encode({ type: "reply", id: req.id, ok: true, result })) || messages.length <= 1) {
        break;
      }
      messages = messages.slice(Math.floor(messages.length / 2));
      truncated = true;
      more = true;
    }
    this.reply(req.id, { messages, truncated, more });
  }

  private async handleStartDaemon(id: number): Promise<void> {
    let running: boolean;
    try {
      running = await this.deps.startDaemon();
    } catch (e) {
      this.error("daemon-start-failed", errText(e));
      running = false;
    }
    this.reply(id, { running });
  }

  private async handleSend(req: Extract<Request, { type: "send" }>): Promise<void> {
    const store = this.openStore();
    if (store === null) {
      this.replyError(req.id, { code: "no-store", message: NO_STORE });
      return;
    }
    let replied = false;
    const created = (row: SendRow): void => {
      if (replied || row.id === undefined) return;
      replied = true;
      this.tracked.set(row.id, {
        sessionId: row.sessionId,
        state: row.state,
        detail: row.detail,
        since: this.deps.now(),
      });
      this.reply(req.id, { sendId: row.id, state: row.state });
    };
    const sendReq: SendRequest = { sessionId: req.sessionId, text: req.text };
    if (req.override !== undefined) sendReq.override = req.override;
    if (req.allowSecret !== undefined) sendReq.allowSecret = req.allowSecret;
    try {
      const row = await this.deps.send(sendReq, created);
      created(row);
      // The relay's own writes go through this connection and do not move data_version.
      this.checkSends(store);
    } catch (e) {
      if (replied) {
        this.error("send-error", errText(e));
        return;
      }
      if (e instanceof SendRefused) {
        const error: ReplyError = {
          code: e.reason,
          message: e.reason === "looks-secret" ? refusalText("looks-secret") : e.message,
        };
        if (e.reason === "send-in-flight") {
          try {
            const open = store.openSendFor(req.sessionId);
            if (open?.id !== undefined) error.sendId = open.id;
          } catch {
            // the refusal still answers the request; only the open send's id is missing
          }
        }
        this.replyError(req.id, error);
        return;
      }
      this.replyError(req.id, { code: "send-error", message: errText(e) });
    }
  }
}

/**
 * `hub bridge`: the tray app's data link. Writes hello and a snapshot, then answers requests from
 * `io.lines` and polls the index for changes until the input ends or a shutdown request arrives.
 * Returns the exit code (0).
 */
export function runBridge(io: BridgeIo, deps: BridgeDeps): Promise<number> {
  return new Bridge(io, deps).run();
}
