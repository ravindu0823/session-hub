import { describe, expect, test } from "bun:test";
import { join } from "node:path";
import { extractStatus } from "../../src/status/extract";

const fx = (name: string) =>
  Bun.file(join(import.meta.dir, "..", "fixtures", "replies", name)).text();

describe("extractStatus", () => {
  test("full reply", async () => {
    const s = extractStatus(await fx("full.md"));
    expect(s).not.toBeNull();
    expect(s?.progressPct).toBe(25);
    expect(s?.phase).toBe("build, Task 1 of 15 (scaffold) running");
    expect(s?.remaining).toEqual([
      "Tasks 1-13: build with review (Task 1 running now).",
      "Task 14: install on this PC and real end-to-end test.",
    ]);
    expect(s?.waiting.map((w) => w.id)).toEqual(["SH1", "SH9", "Q15", "NT56", null]);
    expect(s?.waiting[0]).toEqual({
      id: "SH1",
      text: "SH1–SH8: answered.",
      isNew: false,
      children: [],
    });
    expect(s?.waiting[1]).toEqual({
      id: "SH9",
      text: "Do you approve the design doc as written?",
      isNew: true,
      children: [],
    });
    expect(s?.waiting[2]?.text).toBe("Pick a region (Recommended)");
    expect(s?.waiting[3]?.text).toBe("push the plan");
    expect(s?.waiting[4]).toEqual({
      id: null,
      text: "Plain question without an id",
      isNew: false,
      children: [],
    });
    expect(s?.waiting.some((w) => w.text.includes("fence"))).toBe(false);
  });

  test("recommendations", async () => {
    const s = extractStatus(await fx("full.md"));
    expect(s?.recommendations).toEqual([
      "Scaffold is in. I recommend keeping Bun for the runner.",
      "Q15 Pick a region (Recommended)",
    ]);
  });

  test("recommendations deduped and capped at 5", () => {
    const body = Array.from({ length: 8 }, (_, i) => `I recommend option ${i}`).join("\n");
    const s = extractStatus(
      `**Progress: about 5% overall. Phase: x.**\n${body}\nI recommend option 0\n`,
    );
    expect(s?.recommendations).toHaveLength(5);
    expect(new Set(s?.recommendations).size).toBe(5);
    const t = extractStatus(
      `**Progress: about 5% overall. Phase: x.**\nMy Pick: A\nMy pick: A\nmy pick: A\n`,
    );
    expect(t?.recommendations).toEqual(["My Pick: A", "My pick: A", "my pick: A"]);
  });

  test("nested waiting bullets, curly apostrophe, no 'about'", async () => {
    const s = extractStatus(await fx("nested.md"));
    expect(s?.progressPct).toBe(60);
    expect(s?.phase).toBe("CI setup");
    expect(s?.remaining).toEqual(["Wire the runner."]);
    expect(s?.waiting).toEqual([
      {
        id: "SH10",
        text: "Before CI can run:",
        isNew: true,
        children: ["create the repo", "register the runner with the token"],
      },
      { id: "BM3", text: "Confirm label", isNew: false, children: [] },
    ]);
  });

  test("nothing means empty lists", async () => {
    const s = extractStatus(await fx("nothing.md"));
    expect(s?.progressPct).toBe(100);
    expect(s?.remaining).toEqual([]);
    expect(s?.waiting).toEqual([]);
  });

  test("nothing in any case with period", () => {
    const s = extractStatus("### What's remaining\nNOTHING.\n### Waiting on you\n- Nothing\n");
    expect(s?.remaining).toEqual([]);
    expect(s?.waiting).toEqual([]);
  });

  test("one-line answer is null", async () => {
    expect(extractStatus(await fx("oneliner.md"))).toBeNull();
    expect(extractStatus("")).toBeNull();
  });

  test("bold-line headings and bullet kinds", async () => {
    const s = extractStatus(await fx("bold-headings.md"));
    expect(s?.progressPct).toBeNull();
    expect(s?.phase).toBeNull();
    expect(s?.remaining).toEqual(["Do the thing.", "Do the other thing."]);
    expect(s?.waiting.map((w) => w.id)).toEqual(["Q2", "Q3"]);
  });

  test("section ends at next heading", () => {
    const s = extractStatus(
      "### What's remaining\n1. A\n## Other\n2. B\n### Waiting on you\n- Q1 ask\n",
    );
    expect(s?.remaining).toEqual(["A"]);
    expect(s?.waiting).toHaveLength(1);
  });

  test("bold-line heading of other section ends section", () => {
    const s = extractStatus("**What's remaining**\n1. A\n**Waiting on you**\n- Q1 ask\n");
    expect(s?.remaining).toEqual(["A"]);
    expect(s?.waiting[0]?.id).toBe("Q1");
  });

  test("progress clamped and fence ignored", () => {
    expect(extractStatus("**Progress: about 250% overall. Phase: p.**")?.progressPct).toBe(100);
    expect(extractStatus("```\n**Progress: about 50% overall. Phase: p.**\n```\n")).toBeNull();
  });

  test("IDs", () => {
    const s = extractStatus(
      "### Waiting on you\n- Q15 a\n- SH10: b\n- NT56: c\n- BM3: d\n- **New:** SH9: e\n- **SH4**: f\n",
    );
    expect(s?.waiting.map((w) => [w.id, w.text, w.isNew])).toEqual([
      ["Q15", "a", false],
      ["SH10", "b", false],
      ["NT56", "c", false],
      ["BM3", "d", false],
      ["SH9", "e", true],
      ["SH4", "f", false],
    ]);
  });

  test("trailing paragraph, thematic break and table do not merge into last item", () => {
    const s = extractStatus(
      "### Waiting on you\n- Q1 approve x\n\nThanks, ping me when ready.\n\n---\nfooter\n",
    );
    expect(s?.waiting).toEqual([{ id: "Q1", text: "approve x", isNew: false, children: [] }]);
    const t = extractStatus("### Waiting on you\n- Q1 approve x\n---\n- Q2 no\n");
    expect(t?.waiting).toHaveLength(1);
    // Outside a template group, a table whose rows hold no ID ends the section, as before.
    const u = extractStatus(
      "### Waiting on you\n- Q1 approve x\n| a | b |\n|---|---|\n| c | d |\n- Q2 no\n",
    );
    expect(u?.waiting).toEqual([{ id: "Q1", text: "approve x", isNew: false, children: [] }]);
  });

  test("indented continuation after a blank line still joins", () => {
    const s = extractStatus("### Waiting on you\n- Q1 approve x\n\n  and the rest\n");
    expect(s?.waiting[0]?.text).toBe("approve x and the rest");
  });

  test("lead-in line before first bullet is dropped", () => {
    const s = extractStatus("### Waiting on you\nThese need you:\n- Q1 a\n- Q2 b\n");
    expect(s?.waiting.map((w) => w.id)).toEqual(["Q1", "Q2"]);
  });

  test("code spans and arithmetic are not corrupted", () => {
    const s = extractStatus(
      "### Waiting on you\n- Q1 approve `src/*.ts` and **bold** and 2 * 3 * 4 and snake_case_name\n",
    );
    expect(s?.waiting[0]?.text).toBe("approve src/*.ts and bold and 2 * 3 * 4 and snake_case_name");
    const t = extractStatus("### Waiting on you\n- Q2 __x__ and _y_ and `**z**`\n");
    expect(t?.waiting[0]?.text).toBe("x and y and **z**");
  });

  test("repeated heading: last with content wins", () => {
    const a = extractStatus("### Waiting on you\n- Q1 first\n### Waiting on you\n- Q2 second\n");
    expect(a?.waiting.map((w) => w.id)).toEqual(["Q2"]);
    const b = extractStatus("### Waiting on you\n- Q1 first\n### Waiting on you\n- nothing\n");
    expect(b?.waiting.map((w) => w.id)).toEqual(["Q1"]);
    const c = extractStatus("### What's remaining\n1. A\n### What's remaining\n");
    expect(c?.remaining).toEqual(["A"]);
  });

  test("inline bold headings, bold hash headings and #### level", () => {
    const a = extractStatus("**Waiting on you:** Q5 approve\n- Q6 more\n");
    expect(a?.waiting.map((w) => [w.id, w.text])).toEqual([
      ["Q5", "approve"],
      ["Q6", "more"],
    ]);
    expect(extractStatus("**Waiting on you:** nothing\n")?.waiting).toEqual([]);
    expect(extractStatus("### **Waiting on you**\n- Q1 a\n")?.waiting[0]?.id).toBe("Q1");
    expect(extractStatus("#### What's remaining\n1. A\n")?.remaining).toEqual(["A"]);
  });

  test("ID separators", () => {
    const s = extractStatus(
      "### Waiting on you\n- Q15, Q16: pick\n- **Q15** \u2014 x\n- Q15.5 y\n- Q15x z\n- SH9 - w\n- Q1-Q3 range\n",
    );
    expect(s?.waiting.map((w) => [w.id, w.text])).toEqual([
      ["Q15", "Q16: pick"],
      ["Q15", "x"],
      [null, "Q15.5 y"],
      [null, "Q15x z"],
      ["SH9", "w"],
      ["Q1", "Q1-Q3 range"],
    ]);
  });

  test("remaining children fold into parent", () => {
    const s = extractStatus(
      "### What's remaining\n1. Build\n   - a\n   - b\n2. Before CI:\n   - c\n3. Plain\n",
    );
    expect(s?.remaining).toEqual(["Build: a; b", "Before CI: c", "Plain"]);
  });

  test("progress anchored at line start, decimals rounded", () => {
    expect(extractStatus("**Progress: about 33.5% overall. Phase: p.**")?.progressPct).toBe(34);
    expect(extractStatus("see **Progress: about 50% overall. Phase: p.** quoted")).toBeNull();
    expect(extractStatus("Progress: 40% overall. Phase: plain.")?.phase).toBe("plain");
  });

  test("bold around ids and whole items", () => {
    const s = extractStatus(
      "### Waiting on you\n- **Q15:** text\n- **Q15 approve x**\n- **New:** **SH9:** go\n- **New: SH9** foo\n- SH1\u2013SH8: answered.\n",
    );
    expect(s?.waiting.map((w) => [w.id, w.text, w.isNew])).toEqual([
      ["Q15", "text", false],
      ["Q15", "approve x", false],
      ["SH9", "go", true],
      ["SH9", "foo", true],
      ["SH1", "SH1\u2013SH8: answered.", false],
    ]);
  });

  test("code span at item start is never an id", () => {
    const s = extractStatus(
      "### Waiting on you\n- `Q15` stays open\n- `S3 bucket` needs a policy\n- `V8` upgrade\n- `H2: foo`\n- _Q15_ foo\n",
    );
    expect(s?.waiting.map((w) => [w.id, w.text])).toEqual([
      [null, "Q15 stays open"],
      [null, "S3 bucket needs a policy"],
      [null, "V8 upgrade"],
      [null, "H2: foo"],
      ["Q15", "foo"],
    ]);
  });
  test("hyphenated and multi-segment ids", () => {
    const s = extractStatus(
      [
        "### Waiting on you",
        "- AUTH-Q1 pick a token lifetime",
        "- DB-Q1: which backup window",
        "- AB2-Q3, tool version",
        "- Q14 a",
        "- SH29 b",
        "- NT40 c",
        "- **AUTH-Q1:** bold id",
        "- New: **Q15** fresh",
        "- **New:** **DB-Q2** — also fresh",
        "",
      ].join("\n"),
    );
    expect(s?.waiting.map((w) => [w.id, w.text, w.isNew])).toEqual([
      ["AUTH-Q1", "pick a token lifetime", false],
      ["DB-Q1", "which backup window", false],
      ["AB2-Q3", "tool version", false],
      ["Q14", "a", false],
      ["SH29", "b", false],
      ["NT40", "c", false],
      ["AUTH-Q1", "bold id", false],
      ["Q15", "fresh", true],
      ["DB-Q2", "also fresh", true],
    ]);
  });

  test("words that are not ids", () => {
    for (const item of [
      "OK go",
      "Q alone",
      "A- dash",
      "AUTH- dash",
      "2FA on",
      "q1 lower",
      "JavaScript upgrade",
      "AUTH-q1 lower",
      "AUTH-QA no digit",
      "AUTH--Q1 double dash",
      "E2E tests",
      "ABCDEFGHI1 segment too long",
      "AB1-CD2-EF3-GH4-IJ5-KL6-MN7 longer than 24",
    ]) {
      const s = extractStatus(`### Waiting on you\n- ${item}\n`);
      expect([item, s?.waiting[0]?.id]).toEqual([item, null]);
    }
  });

  test("ranges keep the first id and the whole text, with the new id shapes", () => {
    const s = extractStatus(
      [
        "### Waiting on you",
        "- Q5–Q8: answered.",
        "- Q1-Q3 range",
        "- AUTH-Q1–AUTH-Q4: answered.",
        "- AUTH-Q1-AUTH-Q3 hyphen range",
        "- AUTH-Q1-Q3 short range",
        "- AUTH-Q3-Q1 not ascending",
        "- AUTH-Q1-XQ3 other stem",
        "",
      ].join("\n"),
    );
    expect(s?.waiting.map((w) => [w.id, w.text])).toEqual([
      ["Q5", "Q5–Q8: answered."],
      ["Q1", "Q1-Q3 range"],
      ["AUTH-Q1", "AUTH-Q1–AUTH-Q4: answered."],
      ["AUTH-Q1", "AUTH-Q1-AUTH-Q3 hyphen range"],
      ["AUTH-Q1", "AUTH-Q1-Q3 short range"],
      ["AUTH-Q3-Q1", "not ascending"],
      ["AUTH-Q1-XQ3", "other stem"],
    ]);
  });
});

describe("the standard Waiting on you template (grouped, with tables)", () => {
  test("every group is read: bullets, table rows with the ID in any cell, new marks", async () => {
    const s = extractStatus(await fx("waiting-template.md"));
    expect(s?.progressPct).toBe(60);
    expect(s?.remaining).toEqual(["Rebuild (running now)."]);
    expect(s?.waiting).toEqual([
      {
        id: "SH83",
        text: "Type in the test tab What: type any line in the test tab. I recommend: yes, because it proves the hook. Reply: ok · where: test tab",
        isNew: true,
        children: [],
      },
      {
        id: "AUTH-Q4",
        text: "Rotate the API key · Tab: api · I recommend: Rotate now",
        isNew: false,
        children: [],
      },
      {
        id: "SH85",
        text: "Publish the report · I recommend: Yes · Reply: SH85 yes",
        isNew: true,
        children: [],
      },
      {
        id: "SH86",
        text: "Keep the old flag · I recommend: No, because unused · Reply: SH86 no",
        isNew: false,
        children: [],
      },
      { id: "KIT-Q12", text: "Upgrade the kit · waits on 2026-12-01", isNew: false, children: [] },
    ]);
  });

  test("the count after the heading is not an item; nothing still means none", () => {
    const s = extractStatus("**Waiting on you** — 1 item (🔴 0 urgent · 🆕 1 new)\n\n- SH1 · a\n");
    expect(s?.waiting.map((w) => w.id)).toEqual(["SH1"]);
    expect(extractStatus("**Waiting on you:** nothing.\n")?.waiting).toEqual([]);
  });

  test("settled items are not open, also as bullets under the settled header", () => {
    const s = extractStatus(
      "### Waiting on you\n🟢 **Here**\n- SH1 · a\n\n✅ **Settled since last reply:**\n- SH2 → yes\n",
    );
    expect(s?.waiting.map((w) => w.id)).toEqual(["SH1"]);
  });

  test("outside a group any table ends the section, as before; in a group rows are items", () => {
    const s = extractStatus("### Waiting on you\n| SH1 | approve x |\n| nothing to do | here |\n");
    expect(s?.waiting).toEqual([]);
    const g = extractStatus(
      "### Waiting on you\n🟢 **Here**\n| SH1 | approve x |\n| nothing to do | here |\n",
    );
    expect(g?.waiting).toEqual([
      { id: "SH1", text: "approve x", isNew: false, children: [] },
      { id: null, text: "nothing to do · here", isNew: false, children: [] },
    ]);
  });

  test("tables still end the What's remaining section, and table rows are not recommendations", () => {
    const s = extractStatus(
      "### What's remaining\n- a\n| x | y |\n- b\n\n### Waiting on you\n🟢 **Here**\n| ID | I recommend |\n|---|---|\n| SH1 | yes |\n",
    );
    expect(s?.remaining).toEqual(["a"]);
    expect(s?.recommendations).toEqual([]);
  });

  test("a pipe inside a code span does not split a cell", () => {
    const s = extractStatus(
      "### Waiting on you\n🟢 **Here**\n| ID | Reply |\n|---|---|\n| SH1 | `a | b` |\n",
    );
    expect(s?.waiting[0]?.text).toBe("Reply: a | b");
  });
});

describe("the standard template: review fixes", () => {
  const ids = (md: string) => extractStatus(md)?.waiting.map((w) => w.id);
  const timed = (md: string) => {
    const t0 = performance.now();
    const s = extractStatus(md);
    return { s, ms: performance.now() - t0 };
  };

  test("an indented ✅ line or a ✅ item in a plain list is not the settled group", () => {
    expect(
      ids("**Waiting on you:**\n- SH5 approve release\n  ✅ tests pass\n- SH6 rotate key\n"),
    ).toEqual(["SH5", "SH6"]);
    expect(ids("### Waiting on you\nSH1 approve\n✅ SH2 done\n")).toEqual(["SH1"]);
    expect(ids("### Waiting on you\n- SH1 a\n- ✅ SH2 done\n- SH3 c\n")).toEqual([
      "SH1",
      null,
      "SH3",
    ]);
  });

  test("🆕 inside the bold keeps the ID, in a bullet and in a table cell", () => {
    const s = extractStatus(
      "### Waiting on you\n🔴 **Urgent**\n- **SH84 🆕** do x\n\n🟢 **Here**\n| ID | What it is |\n|---|---|\n| **SH85 🆕** | Publish |\n",
    );
    expect(s?.waiting).toEqual([
      { id: "SH84", text: "do x", isNew: true, children: [] },
      { id: "SH85", text: "Publish", isNew: true, children: [] },
    ]);
  });

  test("nothing, then a table that is not about waiting: still nothing", () => {
    const s = extractStatus(
      "**Waiting on you:** nothing.\n\n| Step | Status |\n|---|---|\n| build | done |\n",
    );
    expect(s?.waiting).toEqual([]);
  });

  test("an indented options table under an item becomes that item's children", () => {
    const s = extractStatus(
      "### Waiting on you\n🟢 **Here**\n- SH1 pick a plan\n  | Plan | Cost |\n  |---|---|\n  | A | 5 |\n  | B | 9 |\n- SH2 b\n",
    );
    expect(s?.waiting).toEqual([
      {
        id: "SH1",
        text: "pick a plan",
        isNew: false,
        children: ["Plan: A · Cost: 5", "Plan: B · Cost: 9"],
      },
      { id: "SH2", text: "b", isNew: false, children: [] },
    ]);
  });

  test("hostile rows: empty, ID only, unclosed backtick, escaped trailing pipe", () => {
    // A plain string: Bun escapes non-ASCII in String.raw templates, which would change the input.
    const s = extractStatus(
      "### Waiting on you\n🟢 **Here**\n| | |\n|\n| SH1 |\n| SH2 | `a | b |\n| SH3 | x \\| y \\|\n",
    );
    expect(s?.waiting).toEqual([
      { id: "SH1", text: "SH1", isNew: false, children: [] },
      { id: "SH2", text: "`a | b", isNew: false, children: [] },
      { id: "SH3", text: "x | y |", isNew: false, children: [] },
    ]);
  });

  test("huge rows and whitespace runs stay linear", () => {
    const wide = `### Waiting on you\n🟢 **Here**\n|${" SH1 |".repeat(1)}${" x |".repeat(10_000)}\n`;
    expect(timed(wide).ms).toBeLessThan(1_000);
    const big = `### Waiting on you\n🟢 **Here**\n| SH1 | ${"a".repeat(1_000_000)} |\n`;
    expect(timed(big).ms).toBeLessThan(1_000);
    const spaces = `### Waiting on you\n- SH1 a\n${" ".repeat(100_000)}x\n${" ".repeat(100_000)}\n`;
    expect(timed(spaces).ms).toBeLessThan(1_000);
    const cell = `### Waiting on you\n🟢 **Here**\n| SH1 🆕${" ".repeat(100_000)}🆕 | b${" ".repeat(100_000)}🆕x |\n`;
    expect(timed(cell).ms).toBeLessThan(1_000);
  });

  test("a lead-in line in a later group is dropped, and later groups are kept", () => {
    expect(
      ids(
        "### Waiting on you\n🔴 **Urgent**\n- SH1 a\n\n🟢 **Here**\nType one reply per line:\n- SH2 b\n",
      ),
    ).toEqual(["SH1", "SH2"]);
    expect(
      ids("### Waiting on you\n🔴 **Urgent**\n- SH1 a\n🟢 **Here**\nlead-in\n- SH2 b\n"),
    ).toEqual(["SH1", "SH2"]);
    expect(
      extractStatus("### Waiting on you\n🔴 **Urgent**\n- SH1 a\n🟢 **Here**\nlead-in\n- SH2 b\n")
        ?.waiting[0]?.text,
    ).toBe("a");
  });

  test("only a bare count after the heading is dropped; the hash heading may carry a count", () => {
    expect(ids("**Waiting on you** — 1 item: SH1 approve deploy\n")).toEqual(["SH1"]);
    expect(ids("**Waiting on you** — 2 items (🔴 1 urgent)\n- SH1 a\n- SH2 b\n")).toEqual([
      "SH1",
      "SH2",
    ]);
    expect(ids("## Waiting on you — 5 items\n- SH1 a\n")).toEqual(["SH1"]);
  });

  test("a group mark in a markdown heading is a heading: it ends the section", () => {
    expect(ids("### Waiting on you\n- SH0 z\n#### 🔴 Urgent\n- SH1 a\n")).toEqual(["SH0"]);
  });

  test("the ID column wins over another cell that looks like an ID", () => {
    const s = extractStatus(
      "### Waiting on you\n🟢 **Here**\n| When | ID | What it is |\n|---|---|---|\n| Q4 | SH9 | Do it |\n",
    );
    expect(s?.waiting[0]?.id).toBe("SH9");
  });

  test("rows outside a waiting section still give recommendations", () => {
    const s = extractStatus(
      "| Plan | Note |\n|---|---|\n| A | (recommended) |\n\n**Waiting on you:** nothing.\n",
    );
    expect(s?.recommendations.length).toBe(1);
  });
});

describe("the standard template: second review fixes", () => {
  const ids = (md: string) => extractStatus(md)?.waiting.map((w) => w.id);
  const timed = (md: string) => {
    const t0 = performance.now();
    extractStatus(md);
    return performance.now() - t0;
  };

  test("an indented bold group mark under an item is never a group header", () => {
    expect(ids("### Waiting on you\n- SH5 a\n  ✅ **tests pass**\n- SH6 b\n")).toEqual([
      "SH5",
      "SH6",
    ]);
    const s = extractStatus(
      "### Waiting on you\n- SH5 a\n  🔴 **careful**\n  more text\n- SH6 b\n",
    );
    expect(s?.waiting.map((w) => w.id)).toEqual(["SH5", "SH6"]);
    expect(s?.waiting[0]?.text).toBe("a 🔴 careful more text");
  });

  test("nothing, then a table with an ID column: still nothing", () => {
    expect(
      extractStatus("**Waiting on you:** nothing.\n\n| ID | State |\n|---|---|\n| SH1 | fixed |\n")
        ?.waiting,
    ).toEqual([]);
  });

  test("a header-only table outside a group ends the section", () => {
    expect(ids("### Waiting on you\n- Q1 approve x\n| a | b |\n|---|---|\n- Q2 no\n")).toEqual([
      "Q1",
    ]);
  });

  test("a count with a colon keeps the item after it", () => {
    const s = extractStatus("**Waiting on you** — 1 item: SH1 approve deploy\n");
    expect(s?.waiting).toEqual([{ id: "SH1", text: "approve deploy", isNew: false, children: [] }]);
  });

  test("a big header with many rows stays linear", () => {
    const head = `|${" h |".repeat(10_000)}\n|${"---|".repeat(10_000)}\n`;
    const rows = "| SH1 | x |\n".repeat(10_000);
    expect(timed(`### Waiting on you\n🟢 **Here**\n${head}${rows}`)).toBeLessThan(1_000);
  });

  test("a plain ID line in a group is an item, a plain lead-in is not", () => {
    expect(
      ids(
        "### Waiting on you\n🔴 **Urgent**\n- SH1 a\n\n⚪ **Can wait**\nKIT-Q12 upgrade the kit\n",
      ),
    ).toEqual(["SH1", "KIT-Q12"]);
  });

  test("a second separator inside a table body keeps the headers", () => {
    const s = extractStatus(
      "### Waiting on you\n🟢 **Here**\n| ID | Reply |\n|---|---|\n| SH1 | a |\n|---|---|\n| SH2 | b |\n",
    );
    expect(s?.waiting.map((w) => w.text)).toEqual(["Reply: a", "Reply: b"]);
  });

  test("settled table rows are not recommendations", () => {
    const s = extractStatus(
      "### Waiting on you\n✅ **Settled**\n| ID | I recommend |\n|---|---|\n| SH1 | yes |\n",
    );
    expect(s?.recommendations).toEqual([]);
  });

  test("a level-2 heading with a group mark ends the section", () => {
    expect(ids("### Waiting on you\n- SH1 a\n## 🟢 Next steps\n- SH2 b\n")).toEqual(["SH1"]);
  });

  test("a tab-indented table under an item is that item's children", () => {
    const s = extractStatus(
      "### Waiting on you\n🟢 **Here**\n- SH1 pick\n\t| Plan |\n\t|---|\n\t| A |\n",
    );
    expect(s?.waiting[0]?.children).toEqual(["Plan: A"]);
  });

  test("a long run of spaces after the phase stays linear", () => {
    const md = `**Progress: about 5% overall. Phase: x${" ".repeat(100_000)}y\n`;
    expect(timed(md)).toBeLessThan(1_000);
    expect(extractStatus("**Progress: about 5% overall. Phase: release gate.**\n")?.phase).toBe(
      "release gate",
    );
  });
});

describe("the standard template: third review fixes", () => {
  const ids = (md: string) => extractStatus(md)?.waiting.map((w) => w.id);
  const timed = (md: string) => {
    const t0 = performance.now();
    extractStatus(md);
    return performance.now() - t0;
  };

  test("nothing written as a line or a bullet, then an ID table: still nothing", () => {
    const table = "\n\n| ID | State |\n|---|---|\n| SH1 | fixed |\n";
    expect(extractStatus(`### Waiting on you\nnothing.${table}`)?.waiting).toEqual([]);
    expect(extractStatus(`**Waiting on you:**\n- nothing${table}`)?.waiting).toEqual([]);
  });

  test("a plain list, then a separate table with ID rows outside a group: only the list", () => {
    expect(
      ids("**Waiting on you:**\n- Q1 approve\n\n| ID | State |\n|---|---|\n| Q0 | answered |\n"),
    ).toEqual(["Q1"]);
  });

  test("a phase that opens with bold still parses, as before", () => {
    const s = extractStatus("**Progress: about 50% overall. Phase: **release gate**.\n");
    expect(s?.progressPct).toBe(50);
    expect(s?.phase).not.toBeNull();
  });

  test("groups of plain ID lines (no bullets, no tables) give one item each", () => {
    expect(
      ids("**Waiting on you**\n🔴 **Urgent**\nSH1 approve deploy\n\n🟢 **Here**\nSH2 publish\n"),
    ).toEqual(["SH1", "SH2"]);
  });

  test("whitespace runs before a \r or a line separator stay linear", () => {
    const pad = " ".repeat(100_000);
    for (const end of ["\r", " "]) {
      expect(timed(`**Progress: about 5% overall. Phase:${pad}x${end}\n`)).toBeLessThan(1_000);
      expect(timed(`**Waiting on you:**${pad}x${end}\n`)).toBeLessThan(1_000);
      expect(timed(`### Waiting on you\n- ${pad}x${end}\n`)).toBeLessThan(1_000);
    }
    expect(extractStatus("**Progress: about 5% overall. Phase: release gate.**\r\n")?.phase).toBe(
      "release gate",
    );
  });

  test("a table as the very last line, in a group, with no newline", () => {
    expect(ids("### Waiting on you\n🟢 **Here**\n| SH1 | x |")).toEqual(["SH1"]);
  });

  test("lines in the settled group are never recommendations", () => {
    const s = extractStatus("### Waiting on you\n✅ **Settled**\n- SH1 → yes (recommended)\n");
    expect(s?.recommendations).toEqual([]);
  });
});

describe("the standard template: final review fixes", () => {
  const ids = (md: string) => extractStatus(md)?.waiting.map((w) => w.id);

  test("a note paragraph between groups does not end the section", () => {
    expect(
      ids(
        "### Waiting on you\n🔴 **Urgent**\n- SH1 a\n\nThis blocks the release.\n\n🟡 **Later**\n- SH2 b\n",
      ),
    ).toEqual(["SH1", "SH2"]);
    expect(
      ids(
        "### Waiting on you\n🟢 **Here**\n| ID | What |\n|---|---|\n| SH1 | a |\n\nNote.\n\n⚪ **Can wait**\n- SH2 b\n",
      ),
    ).toEqual(["SH1", "SH2"]);
  });

  test("two plain ID lines in one group, split by a blank line: two items", () => {
    expect(ids("**Waiting on you**\n🔴 **Urgent**\nSH1 approve\n\nSH2 pick\n")).toEqual([
      "SH1",
      "SH2",
    ]);
  });

  test("a mark line whose bold text is an ID is an item line, as on main", () => {
    expect(ids("### Waiting on you\n- SH1 a\n✅ **SH2** answered\n- SH3 c\n")).toEqual([
      "SH1",
      "SH3",
    ]);
    expect(ids("### Waiting on you\n🔴 **SH1** approve deploy\n🟡 **SH2** pick date\n")).toEqual([
      null,
    ]);
  });

  test("the settled group ends at a paragraph after a blank line", () => {
    const s = extractStatus(
      "### Waiting on you\n✅ **Settled**\n- SH0 yes\n\nI recommend shipping today.\n",
    );
    expect(s?.waiting).toEqual([]);
    expect(s?.recommendations).toEqual(["I recommend shipping today."]);
  });

  test("a group of one plain line with no ID, after a table group, is kept", () => {
    const s = extractStatus(
      "### Waiting on you\n🟢 **Here**\n| ID | What |\n|---|---|\n| SH1 | a |\n\n🟡 **Later**\nDecide the release date.\n",
    );
    expect(s?.waiting.map((w) => w.text)).toEqual(["a", "Decide the release date."]);
  });

  test("a second table right under the first in a group starts a new header", () => {
    const s = extractStatus(
      "### Waiting on you\n🟢 **Here**\n| ID | What |\n|---|---|\n| SH1 | a |\n| ID | Reply |\n|---|---|\n| SH2 | b |\n",
    );
    expect(s?.waiting.map((w) => [w.id, w.text])).toEqual([
      ["SH1", "a"],
      ["SH2", "Reply: b"],
    ]);
  });

  test("an open group's title is not a recommendation", () => {
    expect(
      extractStatus("### Waiting on you\n🟢 **Recommended next**\n- SH1 a\n")?.recommendations,
    ).toEqual([]);
  });

  test("a whitespace-only phase still gives the progress, as on main", () => {
    expect(extractStatus("**Progress: about 50% overall. Phase:  \n")?.progressPct).toBe(50);
  });
});
