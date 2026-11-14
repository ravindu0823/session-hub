import { Database } from "bun:sqlite";
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  DELIVERY_WINDOW_MS,
  EVAL_SESSION_PREFIX,
  MESSAGE_HEADER,
  RELAY_NAME,
} from "../../src/config.ts";
import { type Classified, classify } from "../../src/ingest/records.ts";
import {
  buildMessage,
  checkDelivery,
  deliveryChecker,
  deliveryMatch,
  expireSends,
  normalize,
  sha256Hex,
} from "../../src/send/verify.ts";
import { type SendRow, type SendState, Store } from "../../src/store/db.ts";

const T0 = Date.parse("2026-11-13T10:00:00.000Z");
const iso = (ms: number): string => new Date(ms).toISOString();

function sendFor(text: string, over: Partial<SendRow> = {}): SendRow {
  return {
    id: 1,
    sessionId: "s1",
    profile: "claude",
    name: "alpha",
    sha256: sha256Hex(normalize(buildMessage(text))),
    createdAt: T0,
    state: "pending",
    detail: null,
    ...over,
  };
}

function peer(body: string, ts: number, fromName = RELAY_NAME, uuid = `p-${ts}`): Classified {
  return { kind: "peer-message", uuid, ts: iso(ts), fromName, fromMode: "bypass", body };
}

function held(ts: number, peerName = RELAY_NAME): Classified {
  return { kind: "held-notice", uuid: `h-${ts}`, ts: iso(ts), peerName };
}

describe("buildMessage / normalize", () => {
  test("header, blank line, then the text unchanged", () => {
    expect(buildMessage("hi\nthere")).toBe(`${MESSAGE_HEADER}\n\nhi\nthere`);
  });
  test("normalize turns CRLF into LF and trims trailing whitespace only", () => {
    expect(normalize("  a\r\nb \r\n\r\n \t")).toBe("  a\nb");
    expect(normalize("x")).toBe("x");
  });
  test("sha256Hex is hex SHA-256 of UTF-8", () => {
    expect(sha256Hex("é😀")).toBe(createHash("sha256").update("é😀", "utf8").digest("hex"));
  });
});

describe("checkDelivery", () => {
  const text = 'yes, go ahead 👍️ "quoted" `code`\nsecond line';
  const body = buildMessage(text); // the transcript body has the trailing newline removed

  test("delivered on the exact body from the relay after createdAt", () => {
    expect(checkDelivery(sendFor(text), [peer(body, T0 + 5000)], T0 + 6000)).toBe("delivered");
  });
  test("delivered from both transcript shapes: busy (queued_command) and idle (peer user record)", () => {
    const busy = JSON.stringify({
      type: "attachment",
      uuid: "b-1",
      timestamp: iso(T0 + 5000),
      attachment: {
        type: "queued_command",
        prompt: `<cross-session-message>\n${body}\n</cross-session-message>`,
        origin: { kind: "peer", name: RELAY_NAME, fromMode: "bypass", body },
      },
    });
    const idle = JSON.stringify({
      type: "user",
      isMeta: true,
      uuid: "i-1",
      timestamp: iso(T0 + 5000),
      origin: { kind: "peer", from: "uds:fixture", msg_id: "m-1" },
      message: {
        role: "user",
        content: `Another Claude session sent a message:\n<cross-session-message from="uds:fixture" from-name="${RELAY_NAME}" from-mode="bypass">\n${body}\n</cross-session-message>\n\nfixture hint`,
      },
    });
    for (const line of [busy, idle]) {
      expect(checkDelivery(sendFor(text), [classify(line)], T0 + 6000)).toBe("delivered");
    }
  });
  test("delivered when the relay dropped the header, and only then", () => {
    expect(checkDelivery(sendFor(text), [peer(text, T0 + 5000)], T0 + 6000)).toBe("delivered");
    expect(checkDelivery(sendFor(text), [peer(`${text}\r\n `, T0 + 5000)], T0 + 6000)).toBe(
      "delivered",
    );
    expect(deliveryMatch(sendFor(text), [peer(text, T0 + 5000)])).toBe("headerless");
    expect(deliveryMatch(sendFor(text), [peer(body, T0 + 5000)])).toBe("full");
    // Not the text, a header plus other text, the header alone, or a non-relay sender.
    for (const b of [
      `${text}!`,
      `${MESSAGE_HEADER}\n\nother`,
      MESSAGE_HEADER,
      `${MESSAGE_HEADER}\n\n${MESSAGE_HEADER}\n\n${text}`,
    ]) {
      expect([b, checkDelivery(sendFor(text), [peer(b, T0 + 5000)], T0 + 6000)]).toEqual([
        b,
        "pending",
      ]);
    }
    expect(checkDelivery(sendFor(text), [peer(text, T0 + 5000, "someone")], T0 + 6000)).toBe(
      "pending",
    );
    expect(deliveryMatch(sendFor(text), [peer(text, T0 - 5000)])).toBeNull();
  });
  test("delivered when the body differs only by CRLF / trailing whitespace", () => {
    const crlf = `${body.replaceAll("\n", "\r\n")}\n  `;
    expect(checkDelivery(sendFor(text), [peer(crlf, T0 + 5000)], T0 + 6000)).toBe("delivered");
  });
  test("a record within 1 s clock skew before createdAt still counts", () => {
    expect(checkDelivery(sendFor(text), [peer(body, T0 - 900)], T0)).toBe("delivered");
  });
  test("NOT delivered on identical text that predates createdAt", () => {
    expect(checkDelivery(sendFor(text), [peer(body, T0 - 5000)], T0 + 6000)).toBe("pending");
  });
  test("NOT delivered when the body differs by one character", () => {
    const off = `${body.slice(0, -1)}X`;
    expect(checkDelivery(sendFor(text), [peer(off, T0 + 5000)], T0 + 6000)).toBe("pending");
  });
  test("NOT delivered when the sender is not the relay", () => {
    expect(checkDelivery(sendFor(text), [peer(body, T0 + 5000, "other")], T0 + 6000)).toBe(
      "pending",
    );
  });
  test("NOT delivered on an unparseable timestamp", () => {
    const r: Classified = { ...(peer(body, T0 + 5000) as object), ts: "garbage" } as Classified;
    expect(checkDelivery(sendFor(text), [r], T0 + 6000)).toBe("pending");
  });
  test("held on a held-notice from the relay, then delivered when the body arrives", () => {
    const s = sendFor(text);
    const first = checkDelivery(s, [held(T0 + 3000)], T0 + 4000);
    expect(first).toBe("held");
    const second = checkDelivery({ ...s, state: first }, [peer(body, T0 + 60_000)], T0 + 61_000);
    expect(second).toBe("delivered");
  });
  test("held-notice from another peer or before createdAt is ignored", () => {
    expect(checkDelivery(sendFor(text), [held(T0 + 3000, "someone")], T0 + 4000)).toBe("pending");
    expect(checkDelivery(sendFor(text), [held(T0 - 5000)], T0 + 4000)).toBe("pending");
  });
  test("expired after the delivery window, pending or held", () => {
    const after = T0 + DELIVERY_WINDOW_MS + 1;
    expect(checkDelivery(sendFor(text), [], after)).toBe("expired");
    expect(checkDelivery(sendFor(text, { state: "held" }), [], after)).toBe("expired");
    expect(checkDelivery(sendFor(text), [], T0 + DELIVERY_WINDOW_MS)).toBe("pending");
  });
  test("a late delivery seen in the same pass beats expiry", () => {
    const after = T0 + DELIVERY_WINDOW_MS + 10;
    expect(checkDelivery(sendFor(text), [peer(body, T0 + 600_000)], after)).toBe("delivered");
  });
  test("delivered and expired never change", () => {
    for (const state of ["delivered", "expired"] as SendState[]) {
      const s = sendFor(text, { state });
      expect(checkDelivery(s, [peer(body, T0 + 5000), held(T0 + 1)], T0 + 6000)).toBe(state);
      expect(checkDelivery(s, [], T0 + DELIVERY_WINDOW_MS * 2)).toBe(state);
    }
  });
  test("failed is promoted to delivered on an exact match inside the window, nothing else", () => {
    const s = sendFor(text, { state: "failed" });
    expect(checkDelivery(s, [peer(body, T0 + 5000)], T0 + 6000)).toBe("delivered");
    expect(checkDelivery(s, [peer(body, T0 + 5000)], T0 + DELIVERY_WINDOW_MS)).toBe("delivered");
    expect(checkDelivery(s, [peer(body, T0 + 5000)], T0 + DELIVERY_WINDOW_MS + 1)).toBe("failed");
    expect(checkDelivery(s, [peer(`${body}!`, T0 + 5000)], T0 + 6000)).toBe("failed");
    expect(checkDelivery(s, [peer(body, T0 - 5000)], T0 + 6000)).toBe("failed");
    expect(checkDelivery(s, [held(T0 + 1)], T0 + 6000)).toBe("failed");
    expect(checkDelivery(s, [], T0 + DELIVERY_WINDOW_MS * 2)).toBe("failed");
  });
});

describe("deliveryChecker / expireSends", () => {
  let dir: string;
  let store: Store;
  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), "hub-verify-"));
    store = Store.open(join(dir, "hub.db"));
  });
  afterEach(() => {
    store.close();
    rmSync(dir, { recursive: true, force: true });
  });

  // Read the row through a separate connection, as another process would.
  const state = (id: number): SendState | undefined => {
    const db = new Database(join(dir, "hub.db"), { readonly: true });
    try {
      const r = db.query("SELECT state FROM sends WHERE id = ?").get(id) as {
        state: SendState;
      } | null;
      return r?.state;
    } finally {
      db.close();
    }
  };

  test("updates the store when the target session's transcript shows delivery", () => {
    const text = "deploy it";
    const id = store.createSend(sendFor(text));
    const check = deliveryChecker(store, () => T0 + 6000);
    check("s1", [peer(buildMessage(text), T0 + 5000)]);
    expect(state(id)).toBe("delivered");
    expect(store.openSendFor("s1")).toBeNull();
  });

  test("a delivery without the header is recorded in the detail", () => {
    const id = store.createSend(sendFor("deploy it"));
    const full = store.createSend(sendFor("ship it", { id: 2, sessionId: "s2" }));
    const check = deliveryChecker(store, () => T0 + 6000);
    check("s1", [peer("deploy it", T0 + 5000)]);
    check("s2", [peer(buildMessage("ship it"), T0 + 5000)]);
    expect(store.getSend(id)).toMatchObject({
      state: "delivered",
      detail: "delivered without the Session Hub header",
    });
    expect(store.getSend(full)).toMatchObject({ state: "delivered", detail: null });
  });

  test("a delivery seen in another session's transcript never counts", () => {
    const text = "deploy it";
    const id = store.createSend(sendFor(text));
    deliveryChecker(store, () => T0 + 6000)("s2", [peer(buildMessage(text), T0 + 5000)]);
    expect(state(id)).toBe("pending");
  });

  test("a relay message that matches no send to that session is flagged as stray (LLM-01)", () => {
    const text = "deploy it";
    store.createSend(sendFor(text)); // sent to s1
    const strays: { sessionId: string; ts: string }[] = [];
    const check = deliveryChecker(
      store,
      () => T0 + 6000,
      (s) => strays.push(s),
    );
    // The relay delivered to s2 instead, and also sent a text the owner never wrote.
    check("s2", [peer(buildMessage(text), T0 + 5000)]);
    check("s1", [peer(buildMessage("rm -rf everything"), T0 + 5100)]);
    // A peer message from another session is not the relay's: never flagged.
    check("s1", [peer("hello", T0 + 5200, "other-session")]);
    expect(strays).toEqual([
      { sessionId: "s2", ts: iso(T0 + 5000) },
      { sessionId: "s1", ts: iso(T0 + 5100) },
    ]);
    expect(store.getMeta("relay_stray_count")).toBe("2");
    expect(store.getMeta("relay_stray_last")).toBe(String(T0 + 6000));
  });

  test("relay messages in a relay-eval fixture session are never counted as stray", () => {
    store.upsertSession({
      sessionId: "s-eval",
      profile: "claude",
      name: `${EVAL_SESSION_PREFIX}target-x1`,
      cwd: "C:/x",
      pid: 1,
      kind: "background",
      status: "idle",
      startedAt: T0,
      updatedAt: T0,
      endedAt: null,
      transcriptPath: "C:/x/s-eval.jsonl",
      permissionMode: null,
    });
    const strays: unknown[] = [];
    deliveryChecker(
      store,
      () => T0 + 6000,
      (s) => strays.push(s),
    )("s-eval", [peer(buildMessage("eval case"), T0 + 5000)]);
    expect(strays).toEqual([]);
    expect(store.getMeta("relay_stray_count")).toBeNull();
    expect(store.getMeta("relay_stray_last")).toBeNull();
  });

  test("a relay record older than retention (a re-read transcript) is never flagged", () => {
    const strays: unknown[] = [];
    const now = T0 + 10 * 24 * 3600_000;
    deliveryChecker(
      store,
      () => now,
      (s) => strays.push(s),
    )("s1", [peer(buildMessage("an old send whose row was purged"), T0)]);
    expect(strays).toEqual([]);
    expect(store.getMeta("relay_stray_count")).toBeNull();
  });

  test("a headerless relay body (the relay dropped the header) of a known send is not stray", () => {
    const text = "deploy it";
    store.createSend(sendFor(text, { state: "delivered" }));
    const strays: unknown[] = [];
    deliveryChecker(
      store,
      () => T0 + 6000,
      (s) => strays.push(s),
    )("s1", [peer(text, T0 + 5000)]);
    expect(strays).toEqual([]);
    expect(store.getMeta("relay_stray_count")).toBeNull();
  });

  test("a relay message matching a send to that session, in any state, is not stray", () => {
    const text = "deploy it";
    store.createSend(sendFor(text, { state: "delivered" }));
    const strays: unknown[] = [];
    const check = deliveryChecker(
      store,
      () => T0 + 6000,
      (s) => strays.push(s),
    );
    check("s1", [peer(buildMessage(text), T0 + 5000)]);
    expect(strays).toEqual([]);
    expect(store.getMeta("relay_stray_count")).toBeNull();
  });

  test("held then delivered across passes", () => {
    const text = "ok";
    const id = store.createSend(sendFor(text));
    const check = deliveryChecker(store, () => T0 + 10_000);
    check("s1", [held(T0 + 2000)]);
    expect(state(id)).toBe("held");
    check("s1", []);
    expect(state(id)).toBe("held");
    check("s1", [peer(buildMessage(text), T0 + 9000)]);
    expect(state(id)).toBe("delivered");
  });

  test("two overridden identical sends: one record delivers only the older one", () => {
    const text = "yes";
    const a = store.createSend(sendFor(text, { createdAt: T0 }));
    const b = store.createSend(sendFor(text, { createdAt: T0 + 1000 }));
    const check = deliveryChecker(store, () => T0 + 20_000);
    check("s1", [peer(buildMessage(text), T0 + 5000, RELAY_NAME, "r1")]);
    expect(state(a)).toBe("delivered");
    expect(state(b)).toBe("pending");
    check("s1", [peer(buildMessage(text), T0 + 15_000, RELAY_NAME, "r2")]);
    expect(state(b)).toBe("delivered");
  });

  test("expireSends marks old pending and held sends expired and counts them", () => {
    const old1 = store.createSend(sendFor("a", { sessionId: "s1", createdAt: T0 }));
    const old2 = store.createSend(sendFor("b", { sessionId: "s2", createdAt: T0, state: "held" }));
    const fresh = store.createSend(sendFor("c", { sessionId: "s3", createdAt: T0 + 60_000 }));
    const failed = store.createSend(
      sendFor("d", { sessionId: "s4", createdAt: T0, state: "failed" }),
    );
    const n = expireSends(store, () => T0 + DELIVERY_WINDOW_MS + 1);
    expect(n).toBe(2);
    expect(state(old1)).toBe("expired");
    expect(state(old2)).toBe("expired");
    expect(state(fresh)).toBe("pending");
    expect(state(failed)).toBe("failed");
    expect(expireSends(store, () => T0 + DELIVERY_WINDOW_MS + 1)).toBe(0);
  });

  test("deliveryChecker also expires the session's open send after the window", () => {
    const id = store.createSend(sendFor("x"));
    deliveryChecker(store, () => T0 + DELIVERY_WINDOW_MS + 1)("s1", []);
    expect(state(id)).toBe("expired");
  });

  test("a failed send is promoted to delivered inside the window, not after it", () => {
    const text = "go";
    const early = store.createSend(sendFor(text, { state: "failed", detail: "relay timed out" }));
    deliveryChecker(store, () => T0 + 60_000)("s1", [peer(buildMessage(text), T0 + 50_000)]);
    expect(state(early)).toBe("delivered");
    // The relay's failure reason no longer applies once the message arrived.
    expect(store.getSend(early)?.detail).toBeNull();

    const late = store.createSend(sendFor(text, { sessionId: "s2", state: "failed" }));
    deliveryChecker(store, () => T0 + DELIVERY_WINDOW_MS + 1)("s2", [
      peer(buildMessage(text), T0 + 50_000),
    ]);
    expect(state(late)).toBe("failed");
  });

  test("a held-notice applies only to the newest pending send of the session", () => {
    const older = store.createSend(sendFor("first", { createdAt: T0 }));
    const newer = store.createSend(sendFor("second", { createdAt: T0 + 1000 }));
    deliveryChecker(store, () => T0 + 5000)("s1", [held(T0 + 3000)]);
    expect(state(older)).toBe("pending");
    expect(state(newer)).toBe("held");
  });
});

describe("Store.transitionSend", () => {
  let dir: string;
  let store: Store;
  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), "hub-transition-"));
    store = Store.open(join(dir, "hub.db"));
  });
  afterEach(() => {
    store.close();
    rmSync(dir, { recursive: true, force: true });
  });

  const make = (state: SendState): number => store.createSend(sendFor("t", { state }));
  const all: SendState[] = ["pending", "held", "delivered", "expired", "failed"];

  test("pending and held move to any state", () => {
    for (const from of ["pending", "held"] as SendState[]) {
      for (const to of all) {
        const id = make(from);
        expect(store.transitionSend(id, to)).toBe(true);
        expect(store.getSend(id)?.state).toBe(to);
      }
    }
  });
  test("delivered and expired never change", () => {
    for (const from of ["delivered", "expired"] as SendState[]) {
      for (const to of all) {
        const id = make(from);
        expect(store.transitionSend(id, to, "x")).toBe(false);
        expect(store.getSend(id)).toMatchObject({ state: from, detail: null });
      }
    }
  });
  test("failed moves only to delivered, which clears the failure detail unless one is given", () => {
    for (const to of all.filter((s) => s !== "delivered")) {
      const id = store.createSend(sendFor("t", { state: "failed", detail: "exit 1" }));
      expect(store.transitionSend(id, to)).toBe(false);
      expect(store.getSend(id)).toMatchObject({ state: "failed", detail: "exit 1" });
    }
    const id = store.createSend(sendFor("t", { state: "failed", detail: "exit 1" }));
    expect(store.transitionSend(id, "delivered")).toBe(true);
    expect(store.getSend(id)).toMatchObject({ state: "delivered", detail: null });
    const given = store.createSend(sendFor("t", { state: "failed", detail: "exit 1" }));
    expect(store.transitionSend(given, "delivered", "seen")).toBe(true);
    expect(store.getSend(given)).toMatchObject({ state: "delivered", detail: "seen" });
  });
  test("pending and held keep their detail when none is given", () => {
    for (const from of ["pending", "held"] as SendState[]) {
      const id = store.createSend(sendFor("t", { state: from, detail: "note" }));
      expect(store.transitionSend(id, "delivered")).toBe(true);
      expect(store.getSend(id)).toMatchObject({ state: "delivered", detail: "note" });
    }
  });
  test("detail null clears, a string sets, unknown id changes nothing", () => {
    const id = store.createSend(sendFor("t", { detail: "old" }));
    expect(store.transitionSend(id, "held", null)).toBe(true);
    expect(store.getSend(id)?.detail).toBeNull();
    expect(store.transitionSend(id, "failed", "why")).toBe(true);
    expect(store.getSend(id)?.detail).toBe("why");
    expect(store.transitionSend(9999, "delivered")).toBe(false);
    expect(store.getSend(9999)).toBeNull();
  });
});
