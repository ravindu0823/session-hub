// Protocol fixtures for the .NET tests (ruling R2): runs the real bridge against a temp fixture
// store (never real data) and keeps the exact lines it writes. `bun test/bridge/make-fixtures.ts`
// writes them to gui/SessionHub.Tests/Fixtures/; test/bridge/fixtures.test.ts checks the committed
// files still equal what this produces.

import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { HEARTBEAT_MS, POLL_MS } from "../../src/bridge/protocol";
import { type BridgeDeps, runBridge } from "../../src/bridge/server";
import { MESSAGE_HEADER } from "../../src/config";
import { REFUSE_REASONS, refusalText } from "../../src/send/refusals";
import { MAX_TEXT_CHARS, SendRefused } from "../../src/send/relay";
import { type SendRow, type SessionRow, Store } from "../../src/store/db";
import { FakeClock, FakeIo, flush } from "./harness";

export const FIXTURE_DIR = join(import.meta.dir, "..", "..", "gui", "SessionHub.Tests", "Fixtures");

const T0 = 1_763_000_000_000;
const VERSION = "0.1.0-fixture";

function session(over: Partial<SessionRow> & { sessionId: string; name: string }): SessionRow {
  return {
    profile: "claude",
    cwd: "D:/work/app",
    pid: 4100,
    kind: "interactive",
    status: "idle",
    startedAt: T0 - 3_600_000,
    updatedAt: T0 - 60_000,
    endedAt: null,
    transcriptPath: "D:/fixture/transcript.jsonl",
    permissionMode: "default",
    ...over,
  };
}

/** Sessions, status, messages and meta covering the shapes and hostile text the tray must handle. */
function seed(store: Store): void {
  store.upsertSession(session({ sessionId: "s-alpha", name: "alpha", status: "busy", pid: 4101 }));
  store.setStatus(
    "s-alpha",
    {
      progressPct: 40,
      phase: "build \u001b[31mred\u001b[0m \u202Eevil",
      remaining: ["write tests", 'quote " and backslash \\ stay'],
      waiting: [
        { id: "Q1", text: "Approve the plan?", isNew: true, children: ["option A", "option B"] },
        { id: null, text: "Give the API key handle", isNew: false, children: [] },
      ],
      recommendations: ["merge after review"],
    },
    "r-alpha-1",
    "2025-11-13T02:10:00.000Z",
  );
  store.setLatestReply("s-alpha", "r-alpha-1", "2025-11-13T02:10:00.000Z", true);
  store.upsertSession(
    session({
      sessionId: "s-beta",
      name: "beta-🚀",
      profile: "claude-personal",
      cwd: "D:/work/naïve dir/日本",
      status: "waiting",
      pid: 4102,
      permissionMode: "plan",
    }),
  );
  store.upsertSession(
    session({ sessionId: "s-gamma", name: "gamma", status: "unknown", endedAt: T0 - 7_200_000 }),
  );
  // One message over 256 KiB: history returns it cut, with textTruncated.
  store.upsertSession(session({ sessionId: "s-big", name: "big", pid: 4103 }));
  store.addMessages("s-big", [
    {
      uuid: "big1",
      sessionId: "s-big",
      ts: "2025-11-13T01:00:00.000Z",
      role: "assistant",
      fromName: null,
      text: "big ".repeat(80_000),
    },
  ]);
  store.setMeta("unknown_count", "2");
  store.setMeta("unknown_version", "2.9.0");
  store.setMeta("stale_since:claude-personal", String(T0 - 120_000));
  store.setMeta("relay_stray_count", "1");
  store.setMeta("relay_stray_last", String(T0 - 60_000));
  store.addMessages("s-alpha", [
    {
      uuid: "m1",
      sessionId: "s-alpha",
      ts: "2025-11-13T02:00:00.000Z",
      role: "owner",
      fromName: null,
      text: "deploy the build\nsecond line\nthe password=hunter22 is masked",
    },
    {
      uuid: "m2",
      sessionId: "s-alpha",
      ts: "2025-11-13T02:05:00.000Z",
      role: "peer",
      fromName: "beta-🚀",
      text: 'peer says "hi" \u0007with a bell',
    },
    {
      uuid: "m3",
      sessionId: "s-alpha",
      ts: "2025-11-13T02:10:00.000Z",
      role: "assistant",
      fromName: null,
      text: "**Progress: about 40% overall. Phase: build.** 😀 مرحبا",
    },
  ]);
}

export const REQUESTS: string[] = [
  '{"id":1,"type":"history","sessionId":"s-alpha","limit":50}',
  '{"id":2,"type":"history","sessionId":"s-alpha","limit":50,"beforeTs":"2025-11-13T02:10:00.000Z"}',
  '{"id":3,"type":"search","q":"deploy","limit":20}',
  '{"id":4,"type":"send","sessionId":"s-alpha","text":"hello from the tray\\nsecond line"}',
  '{"id":5,"type":"send","sessionId":"s-alpha","text":"please fail"}',
  '{"id":6,"type":"send","sessionId":"s-beta","text":"are you there?"}',
  '{"id":7,"type":"send","sessionId":"s-beta","text":"are you there?","override":true}',
  '{"id":8,"type":"send","sessionId":"s-alpha","text":"   "}',
  '{"id":9,"type":"startDaemon"}',
  '{"id":10,"type":"ping"}',
  '{"id":11,"type":"history","sessionId":"s-alpha","limit":1}',
  '{"id":12,"type":"history","sessionId":"s-big","limit":10}',
  '{"id":13,"type":"history","sessionId":"s-alpha","limit":50,"beforeTs":"2025-11-13T02:10:00.000Z","beforeUuid":"m3"}',
  '{"type":"shutdown"}',
];

const MALFORMED = ["{not json", '{"type":"focus","sessionId":"s-alpha"}'];

function sendRow(store: Store, clock: FakeClock, sessionId: string): SendRow {
  const r: SendRow = {
    sessionId,
    profile: sessionId === "s-beta" ? "claude-personal" : "claude",
    name: sessionId === "s-beta" ? "beta" : "alpha",
    sha256: "0".repeat(64),
    createdAt: clock.now(),
    state: "pending",
    detail: null,
  };
  return store.tx(() => ({ ...r, id: store.createSend(r) }));
}

async function run(
  store: Store | null,
  daemon: boolean,
  drive: (io: FakeIo, clock: FakeClock) => Promise<void>,
): Promise<FakeIo> {
  const io = new FakeIo();
  const clock = new FakeClock(T0);
  const deps: BridgeDeps = {
    store: () => store,
    daemonRunning: () => daemon,
    startDaemon: async () => true,
    send: async (req, onCreated) => {
      if (store === null) throw new Error("no store");
      if (req.text.trim() === "") throw new SendRefused("empty-message");
      if (!req.override && store.openSendFor(req.sessionId)) {
        throw new SendRefused("send-in-flight");
      }
      const row = sendRow(store, clock, req.sessionId);
      onCreated(row);
      const id = row.id as number;
      if (req.text.includes("fail")) {
        store.transitionSend(id, "failed", "exit 1; result: FAILED: recipient not found");
      }
      return store.getSend(id) as SendRow;
    },
    now: clock.now,
    sleep: clock.sleep,
    version: VERSION,
  };
  const done = runBridge({ lines: io.lines, write: io.write }, deps);
  await flush();
  await drive(io, clock);
  io.end();
  await done;
  return io;
}

export async function generateFixtures(): Promise<Record<string, string>> {
  const dir = mkdtempSync(join(tmpdir(), "hub-fixtures-"));
  const dbPath = join(dir, "hub.db");
  const store = Store.open(dbPath);
  const other = Store.open(dbPath);
  try {
    seed(other);
    const send = (n: number) => REQUESTS[n - 1] as string;
    const io = await run(store, true, async (io, clock) => {
      const req = async (line: string) => {
        io.push(line);
        await flush();
        await flush();
      };
      for (const n of [1, 2, 3, 4]) await req(send(n));
      // The daemon (another connection) proves delivery of send 1 in two steps.
      other.transitionSend(1, "held", "recipient is busy; queued");
      await clock.advance(POLL_MS);
      other.transitionSend(1, "delivered", null);
      await clock.advance(POLL_MS);
      await req(send(5));
      // An open send to s-beta from elsewhere: send 6 is refused, send 7 overrides it.
      other.createSend({
        sessionId: "s-beta",
        profile: "claude-personal",
        name: "beta",
        sha256: "1".repeat(64),
        createdAt: T0,
        state: "pending",
        detail: null,
      });
      await clock.advance(POLL_MS);
      for (const n of [6, 7, 8, 9, 10, 11, 12, 13]) await req(send(n));
      for (const line of MALFORMED) await req(line);
      // Nothing changes for 15 s: a heartbeat instead of a snapshot.
      await clock.advance(HEARTBEAT_MS);
      await req(send(14));
    });
    const missing = await run(null, false, async () => {});
    const lines = (io: FakeIo, type: string) =>
      io.raw.filter((l) => (JSON.parse(l) as { type: string }).type === type);
    const file = (ls: string[]) => `${ls.join("\n")}\n`;
    return {
      "hello.ndjson": file(lines(io, "hello")),
      "heartbeat.ndjson": file(lines(io, "heartbeat")),
      "snapshot.ndjson": file([...lines(io, "snapshot"), ...lines(missing, "snapshot")]),
      "send-events.ndjson": file(lines(io, "send")),
      "replies.ndjson": file(lines(io, "reply")),
      "errors.ndjson": file(lines(io, "error")),
      "requests.ndjson": file(REQUESTS),
      // The plain-English refusal texts the TUI shows; the tray must show the same ones.
      "refusals.ndjson": file(
        REFUSE_REASONS.map((code) => JSON.stringify({ code, text: refusalText(code) })),
      ),
      // What the tray's answer form previews and limits against.
      "constants.ndjson": file([
        JSON.stringify({ messageHeader: MESSAGE_HEADER, maxTextChars: MAX_TEXT_CHARS }),
      ]),
    };
  } finally {
    other.close();
    store.close();
    rmSync(dir, { recursive: true, force: true });
  }
}

if (import.meta.main) {
  const files = await generateFixtures();
  mkdirSync(FIXTURE_DIR, { recursive: true });
  for (const [name, text] of Object.entries(files)) writeFileSync(join(FIXTURE_DIR, name), text);
  process.stderr.write(`wrote ${Object.keys(files).length} files to ${FIXTURE_DIR}\n`);
}
