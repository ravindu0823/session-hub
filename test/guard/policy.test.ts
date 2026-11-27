import { describe, expect, test } from "bun:test";
import { blockReason, insideAny, matchesName, type RoleGuard } from "../../src/guard/policy";

const guard: RoleGuard = {
  denyTools: ["mcp__ops*", "mcp__kv__exec", "WebFetch"],
  denyAgents: ["ops-*", "general-purpose", "Explore"],
  writeOnlyUnder: ["C:\\work\\notes", "C:/Users/me/memory/"],
  reason: "coordinator only: delegate",
};
const call = (tool: string, more: { subagentType?: string; path?: string } = {}) => ({
  tool,
  subagentType: more.subagentType ?? null,
  path: more.path ?? null,
});

describe("matchesName", () => {
  test("exact, or a prefix with a trailing star", () => {
    expect(matchesName("Explore", "Explore")).toBe(true);
    expect(matchesName("Explore", "Explorer")).toBe(false);
    expect(matchesName("ops-*", "ops-main")).toBe(true);
    expect(matchesName("ops-*", "xops-main")).toBe(false);
    expect(matchesName("*", "anything")).toBe(true);
  });
});

describe("insideAny", () => {
  test("inside, equal, case and slash differences", () => {
    expect(insideAny("C:\\work\\notes\\a.md", guard.writeOnlyUnder)).toBe(true);
    expect(insideAny("c:/WORK/notes", guard.writeOnlyUnder)).toBe(true);
    expect(insideAny("C:\\Users\\me\\memory\\x\\y.md", guard.writeOnlyUnder)).toBe(true);
  });

  test("a sibling with the same prefix, a climb out, relative and driveless paths are outside", () => {
    expect(insideAny("C:\\work\\notes-old\\a.md", guard.writeOnlyUnder)).toBe(false);
    expect(insideAny("C:\\work\\notes\\..\\secret.md", guard.writeOnlyUnder)).toBe(false);
    expect(insideAny("notes\\a.md", guard.writeOnlyUnder)).toBe(false);
    expect(insideAny("\\work\\notes\\a.md", guard.writeOnlyUnder)).toBe(false);
    expect(insideAny("D:\\work\\notes\\a.md", guard.writeOnlyUnder)).toBe(false);
  });
});

describe("blockReason", () => {
  test("denied tools, by name or prefix, with the reason", () => {
    expect(blockReason(guard, call("mcp__ops-live__deploy"))).toBe(
      "coordinator only: delegate (tool mcp__ops-live__deploy)",
    );
    expect(blockReason(guard, call("mcp__kv__exec"))).not.toBeNull();
    expect(blockReason(guard, call("mcp__kv__list_handles"))).toBeNull();
    expect(blockReason(guard, call("Read"))).toBeNull();
    expect(blockReason(guard, call("Bash"))).toBeNull();
  });

  test("agent types on Agent and Task; no type means general-purpose", () => {
    expect(blockReason(guard, call("Agent", { subagentType: "ops-main" }))).toContain(
      "agent type ops-main",
    );
    expect(blockReason(guard, call("Task", { subagentType: "Explore" }))).not.toBeNull();
    expect(blockReason(guard, call("Agent"))).toContain("agent type general-purpose");
    expect(blockReason(guard, call("Agent", { subagentType: "reviewer" }))).toBeNull();
    expect(blockReason(guard, call("Read", { subagentType: "Explore" }))).toBeNull();
  });

  test("writes only under the listed folders; a write with no path is blocked", () => {
    expect(blockReason(guard, call("Write", { path: "C:\\work\\notes\\a.md" }))).toBeNull();
    expect(blockReason(guard, call("Edit", { path: "C:\\repo\\src\\a.ts" }))).toContain(
      "Edit outside",
    );
    expect(blockReason(guard, call("NotebookEdit", { path: "C:\\repo\\n.ipynb" }))).not.toBeNull();
    expect(blockReason(guard, call("MultiEdit"))).not.toBeNull();
    expect(blockReason(guard, call("Read", { path: "C:\\repo\\src\\a.ts" }))).toBeNull();
  });

  test("an empty policy blocks nothing", () => {
    const none: RoleGuard = { denyTools: [], denyAgents: [], writeOnlyUnder: [], reason: "x" };
    for (const c of [call("Agent"), call("Write", { path: "Z:\\a" }), call("mcp__ops__x")]) {
      expect(blockReason(none, c)).toBeNull();
    }
  });
});
