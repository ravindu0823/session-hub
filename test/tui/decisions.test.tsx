import { afterEach, describe, expect, test } from "bun:test";
import type { Store } from "../../src/store/db";
import { cleanupDirs, type Harness, mount, seed, tempStore } from "./harness";

let h: Harness | undefined;
afterEach(() => {
  h?.dispose();
  h = undefined;
  cleanupDirs();
});

function ask(s: Store, role: string, question: string): void {
  s.mail().createDecision({
    role,
    question,
    options: ["yes", "no"],
    recommendation: "yes",
    fromSession: null,
    createdAt: 1,
  });
}

function queue(s: Store, to: string): void {
  s.mail().insert({
    thread: "T",
    fromRole: "web",
    fromSession: null,
    toRole: to,
    kind: "task",
    body: "b",
    evidence: null,
    createdAt: 1,
    expiresAt: 9e12,
  });
}

describe("decisions tab", () => {
  test("4 opens the decision list with options, recommendation and how to answer", async () => {
    const s = tempStore();
    seed(s);
    ask(s, "web", "Ship the release?");
    h = mount({ store: s });
    await h.until((f) => f.includes("4 Decisions (1)"));
    await h.press("4");
    const f = await h.until((x) => x.includes("D-0001"));
    expect(f).toContain("D-0001  open  web");
    expect(f).toContain("Ship the release?");
    expect(f).toContain("options: yes | no");
    expect(f).toContain("recommended: yes");
    expect(f).toContain("hub answer D-0001");
  });

  test("answered decisions show the answer; a conflict is flagged", async () => {
    const s = tempStore();
    ask(s, "web", "Ship?");
    s.mail().answerDecision("D-0001", "yes", "tray", 2);
    s.mail().answerDecision("D-0001", "no", "cli", 3);
    h = mount({ store: s });
    await h.press("4");
    const f = await h.until((x) => x.includes("D-0001"));
    expect(f).toContain("answer: yes (tray)");
    expect(f).toContain("1 other answer not applied");
  });

  test("roles with waiting mail and no live session are listed, also with no sessions at all", async () => {
    const s = tempStore();
    queue(s, "docs");
    queue(s, "docs");
    h = mount({ store: s });
    await h.press("4");
    const f = await h.until((x) => x.includes("docs"));
    expect(f).toContain("no decisions");
    expect(f).toContain("start a session named docs: 2 queued");
  });
});
