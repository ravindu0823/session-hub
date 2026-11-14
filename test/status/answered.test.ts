import { describe, expect, test } from "bun:test";
import { answeredIds } from "../../src/status/answered";

const ids = ["AB7", "AB8", "AB9", "Q6", "Q7", "Q14", "XY-Q1", "XY-Q3"];

describe("answeredIds", () => {
  test("an ID that opens a line is answered", () => {
    expect(answeredIds(ids, ["AB7 skip that one for now"])).toEqual(new Set(["AB7"]));
    expect(answeredIds(ids, ["AB8 done, check it"])).toEqual(new Set(["AB8"]));
    expect(answeredIds(ids, ["- AB9: yes"])).toEqual(new Set(["AB9"]));
    expect(answeredIds(ids, ["**AB9** yes"])).toEqual(new Set(["AB9"]));
  });

  test("relay answers and several IDs on one line", () => {
    const relay =
      "[Session Hub: message from the owner.]\n\nAnswers:\nAB9: go with the recommended way";
    expect(answeredIds(ids, [relay])).toEqual(new Set(["AB9"]));
    expect(answeredIds(ids, ["Q6 Yes / Q7 Yes, Q14: no"])).toEqual(new Set(["Q6", "Q7", "Q14"]));
    expect(answeredIds(ids, ["Q6 yes and Q7 no"])).toEqual(new Set(["Q6", "Q7"]));
  });

  test("a question or a mid-sentence mention is not an answer", () => {
    expect(answeredIds(ids, ["What does Q6 mean?"])).toEqual(new Set());
    expect(answeredIds(ids, ["Is Q6 done?"])).toEqual(new Set());
    expect(answeredIds(ids, ["Q6: what happens if I say yes?"])).toEqual(new Set());
    expect(answeredIds(ids, ["please look at Q6 again later"])).toEqual(new Set());
    // a question on one line does not stop an answer on the next
    expect(answeredIds(ids, ["Q6 why?\nQ7 yes"])).toEqual(new Set(["Q7"]));
  });

  test("whole tokens only: Q1 does not answer Q14, Q14 does not answer Q1, XY-Q1 is not Q1", () => {
    expect(answeredIds(["Q1"], ["Q14 yes"])).toEqual(new Set());
    expect(answeredIds(["Q14"], ["Q1 yes"])).toEqual(new Set());
    expect(answeredIds(["Q1"], ["XY-Q1 yes", "XQ1 yes", "Q1x"])).toEqual(new Set());
    expect(answeredIds(["XY-Q1"], ["xy-q1 yes"])).toEqual(new Set());
    expect(answeredIds(["XY-Q1"], ["(XY-Q1) yes"])).toEqual(new Set(["XY-Q1"]));
  });

  test("a range answers every ID of the same stem inside it", () => {
    expect(answeredIds(ids, ["AB7–AB9 all recommended approved"])).toEqual(
      new Set(["AB7", "AB8", "AB9"]),
    );
    expect(answeredIds(ids, ["Q6-Q7 yes"])).toEqual(new Set(["Q6", "Q7"]));
    expect(answeredIds(ids, ["Q6 - Q7 yes"])).toEqual(new Set(["Q6", "Q7"]));
    expect(answeredIds(ids, ["XY-Q1-XY-Q3 ok"])).toEqual(new Set(["XY-Q1", "XY-Q3"]));
    expect(answeredIds(ids, ["XY-Q1-Q3 ok"])).toEqual(new Set(["XY-Q1", "XY-Q3"]));
    // a backwards or cross-stem pair answers only the IDs it names
    expect(answeredIds(ids, ["Q14-Q6 ok"])).toEqual(new Set(["Q14", "Q6"]));
    expect(answeredIds(ids, ["AB7-Q7 ok"])).toEqual(new Set(["AB7", "Q7"]));
  });

  test("no IDs, no texts, or IDs not on the list give nothing", () => {
    expect(answeredIds(ids, ["Ok will go with that recommended way"])).toEqual(new Set());
    expect(answeredIds(ids, [])).toEqual(new Set());
    expect(answeredIds([], ["AB7 yes"])).toEqual(new Set());
    expect(answeredIds(ids, ["AB99 yes"])).toEqual(new Set());
  });

  test("long hostile texts stay fast", () => {
    const chain = "A1-".repeat(34_000);
    const lines = `${"Q6 ".repeat(20_000)}\n`.repeat(5);
    const t0 = performance.now();
    answeredIds(["A1", "Q6"], [chain, lines]);
    expect(performance.now() - t0).toBeLessThan(200);
  });
});
