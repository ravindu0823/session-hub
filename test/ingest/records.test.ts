import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { type Classified, classify } from "../../src/ingest/records";

function lines(name: string): string[] {
  return readFileSync(join(import.meta.dir, "..", "fixtures", "records", name), "utf8")
    .split("\n")
    .filter((l) => l.length > 0);
}
function all(name: string): Classified[] {
  return lines(name).map(classify);
}

describe("classify", () => {
  test("typed user prompt (string and text blocks) -> owner-prompt", () => {
    expect(all("user-typed.jsonl")).toEqual([
      {
        kind: "owner-prompt",
        uuid: "u-1",
        ts: "2026-11-13T08:26:28.698Z",
        text: "fixture owner prompt",
      },
      {
        kind: "owner-prompt",
        uuid: "u-2",
        ts: "2026-11-13T08:26:29.000Z",
        text: "block one\nblock two",
      },
    ]);
  });

  test("tool_result user record is ignored", () => {
    const c = all("user-ignored.jsonl")[0];
    expect(c?.kind).toBe("ignored");
    expect(JSON.stringify(c)).not.toContain("fixture");
  });

  test("meta, sidechain and non-human-origin user records are ignored", () => {
    const r = all("user-ignored.jsonl");
    expect(r[1]?.kind).toBe("ignored");
    expect(r[2]).toEqual({ kind: "ignored", reason: "sidechain" });
    expect(r[3]).toEqual({ kind: "ignored", reason: "non-human-origin" });
  });

  test("assistant joins text blocks, skips thinking and tool_use", () => {
    const r = all("assistant.jsonl");
    expect(r[0]).toEqual({
      kind: "assistant-text",
      uuid: "a-1",
      ts: "2026-11-13T08:27:00.000Z",
      text: "first part\nsecond part",
    });
    expect(r[1]?.kind).toBe("ignored");
    expect(r[2]).toEqual({ kind: "ignored", reason: "sidechain" });
  });

  test("peer delivery -> peer-message using origin.body", () => {
    const r = all("peer.jsonl");
    expect(r[0]).toEqual({
      kind: "peer-message",
      uuid: "at-1",
      ts: "2026-11-13T08:27:08.617Z",
      fromName: "session-hub-relay",
      fromMode: "bypass",
      body: "fixture peer body",
    });
    expect(r[1]?.kind).toBe("ignored");
    expect(r[2]?.kind).toBe("ignored");
    expect(JSON.stringify(r[2])).not.toContain("fixture file body");
  });

  test("peer delivery to an idle session (user record, peer origin) -> peer-message from the wrapper", () => {
    const r = all("peer-idle.jsonl");
    expect(r[0]).toEqual({
      kind: "peer-message",
      uuid: "pi-1",
      ts: "2026-11-13T10:55:01.000Z",
      fromName: "session-hub-relay",
      fromMode: "bypass",
      body: "[Session Hub: fixture header]\n\nfixture idle body\nsecond line",
    });
    expect(r[1]).toEqual({
      kind: "peer-message",
      uuid: "pi-2",
      ts: "2026-11-13T10:55:02.000Z",
      fromName: "fixture-peer",
      fromMode: "default",
      body: "fixture block body",
    });
    // No wrapper: ignored, and its text never leaks. Never an owner prompt.
    expect(r[2]).toEqual({ kind: "ignored", reason: "peer-unwrapped" });
    expect(r[3]).toEqual({ kind: "ignored", reason: "sidechain" });
    expect(r[4]).toEqual({ kind: "ignored", reason: "not-needed" });
    expect(r.some((c) => c.kind === "owner-prompt")).toBe(false);
    expect(JSON.stringify(r)).not.toContain("trailing hint");
  });

  test("held notice -> held-notice with peerName", () => {
    const r = all("held.jsonl");
    expect(r[0]).toEqual({
      kind: "held-notice",
      uuid: "h-1",
      ts: "2026-11-13T08:28:00.000Z",
      peerName: "scratchpad-03",
    });
    expect(r[1]).toEqual({
      kind: "held-notice",
      uuid: "h-2",
      ts: "2026-11-13T08:28:01.000Z",
      peerName: "",
    });
    expect(r[2]?.kind).toBe("ignored");
  });

  test("permission-mode has empty ts and the mode", () => {
    expect(all("mode.jsonl")).toEqual([
      { kind: "permission-mode", ts: "", mode: "bypassPermissions" },
    ]);
  });

  test("never returns text for api-request and the other never-store types", () => {
    for (const l of lines("never-store.jsonl")) {
      const c = classify(l);
      expect(c.kind).toBe("ignored");
      expect(JSON.stringify(c)).not.toContain("fixture");
    }
  });

  test("known-but-unneeded types are ignored, unrecognised types are ignored with their type", () => {
    const r = all("other.jsonl");
    for (const c of r.slice(0, -1)) expect(c).toEqual({ kind: "ignored", reason: "not-needed" });
    expect(r.length).toBe(13);
    expect(r.at(-1)).toEqual({
      kind: "ignored",
      reason: "unrecognized-type",
      type: "brand-new-type",
    });
  });

  test("bad json and non-objects never throw", () => {
    for (const l of ["{not json", "", "42", "null", "[1]", '"s"']) {
      expect(classify(l)).toEqual({ kind: "ignored", reason: "bad-json" });
    }
  });

  test("an unrecognised type is never unknown, even with a version, and leaks no text", () => {
    const c = classify('{"type":"new-shape","version":"2.9.0","text":"fixture x"}');
    expect(c).toEqual({ kind: "ignored", reason: "unrecognized-type", type: "new-shape" });
    expect(JSON.stringify(c)).not.toContain("fixture");
  });

  test("object without a string type is an unrecognised type with empty type", () => {
    expect(classify("{}")).toEqual({ kind: "ignored", reason: "unrecognized-type", type: "" });
  });

  test("present-but-odd origin fails closed", () => {
    const r = all("user-ignored.jsonl");
    for (const c of r.slice(4)) {
      expect(c).toEqual({ kind: "ignored", reason: "non-human-origin" });
    }
    expect(r.length).toBe(7);
  });

  test("odd shapes of parsed types are unknown, other odd records ignored; never leak text", () => {
    const r = all("odd.jsonl");
    expect(r.length).toBe(13);
    for (const c of r) expect(JSON.stringify(c)).not.toContain("fixture");
    // user/assistant message missing, not an object, or content neither string nor array
    expect(r[0]).toEqual({ kind: "unknown", type: "user" });
    expect(r[1]).toEqual({ kind: "unknown", type: "user" });
    expect(r[2]).toEqual({ kind: "unknown", type: "assistant" });
    expect(r[7]).toEqual({ kind: "unknown", type: "user" });
    expect(r[8]).toEqual({ kind: "unknown", type: "assistant" });
    expect(r[9]).toEqual({ kind: "unknown", type: "user" });
    // attachment without a body, queued_command without origin: still ignored
    expect(r[3]).toEqual({ kind: "ignored", reason: "attachment" });
    expect(r[4]).toEqual({ kind: "ignored", reason: "attachment" });
    expect(r[5]).toEqual({ kind: "ignored", reason: "tool-result" });
    // queued_command origin present but not an object
    expect(r[10]).toEqual({ kind: "unknown", type: "attachment" });
    expect(r[11]).toEqual({ kind: "unknown", type: "attachment" });
    // unknown keeps the record's string version for the banner
    expect(r[12]).toEqual({ kind: "unknown", type: "assistant", version: "2.9.0" });
  });

  test("an owner message typed while the session is busy is an owner prompt", () => {
    const rec = (origin: unknown, prompt: unknown) =>
      classify(
        JSON.stringify({
          type: "attachment",
          uuid: "u-q",
          timestamp: "2026-11-14T09:46:16.003Z",
          attachment: {
            type: "queued_command",
            prompt,
            commandMode: "prompt",
            origin,
            humanTurn: true,
          },
        }),
      );
    expect(rec({ kind: "human" }, "AB8 done, check it")).toEqual({
      kind: "owner-prompt",
      uuid: "u-q",
      ts: "2026-11-14T09:46:16.003Z",
      text: "AB8 done, check it",
    });
    expect(rec({ kind: "human" }, [{ type: "text", text: "Q1: yes" }, { type: "image" }])).toEqual({
      kind: "owner-prompt",
      uuid: "u-q",
      ts: "2026-11-14T09:46:16.003Z",
      text: "Q1: yes",
    });
    // empty or missing text, and any other origin (task notifications, hooks), stay ignored
    expect(rec({ kind: "human" }, "")).toEqual({ kind: "ignored", reason: "empty" });
    expect(rec({ kind: "human" }, undefined)).toEqual({ kind: "ignored", reason: "empty" });
    expect(rec({ kind: "task-notification" }, "Q1: yes")).toEqual({
      kind: "ignored",
      reason: "attachment",
    });
    // a human origin counts only on a human turn
    for (const humanTurn of [false, undefined]) {
      const line = JSON.stringify({
        type: "attachment",
        uuid: "u-q",
        timestamp: "2026-11-14T09:46:16.003Z",
        attachment: {
          type: "queued_command",
          prompt: "Q1: yes",
          origin: { kind: "human" },
          humanTurn,
        },
      });
      expect(classify(line)).toEqual({ kind: "ignored", reason: "attachment" });
    }
  });

  test("non-string type is an unrecognised type with empty type", () => {
    expect(all("odd.jsonl")[6]).toEqual({ kind: "ignored", reason: "unrecognized-type", type: "" });
  });

  test("a numeric version is not carried on unknown", () => {
    expect(classify('{"type":"user","message":null,"version":3}')).toEqual({
      kind: "unknown",
      type: "user",
    });
  });

  test("sidechain and meta records are ignored before the shape check", () => {
    expect(classify('{"type":"user","isSidechain":true,"message":null}')).toEqual({
      kind: "ignored",
      reason: "sidechain",
    });
    expect(classify('{"type":"assistant","isSidechain":true}')).toEqual({
      kind: "ignored",
      reason: "sidechain",
    });
  });

  test("classify never throws on odd values in parsed types", () => {
    const odd = ["null", "42", '"s"', "[]", "{}", "true"];
    for (const t of ["user", "assistant", "attachment", "system", "permission-mode"]) {
      for (const v of odd) {
        for (const field of ["message", "attachment", "content", "origin", "permissionMode"]) {
          expect(() => classify(`{"type":"${t}","${field}":${v}}`)).not.toThrow();
        }
      }
    }
  });
});
