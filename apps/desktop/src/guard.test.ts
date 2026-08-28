import { describe, expect, it } from "vitest";
import { isDangerous, NOTHING_SUPPLIED, satisfied } from "./guard";
import type { ConfirmGuard } from "./bindings";

const COUNT: ConfirmGuard = { type: "TypeCount", value: 31 };
const ACK: ConfirmGuard = { type: "Acknowledge", value: "preconditions do not apply" };

describe("satisfied", () => {
  it("lets ordinary work through without ceremony", () => {
    // Extra steps on ordinary work are wallpaper within a week, and then they
    // are not there on the day they matter.
    expect(satisfied(null, NOTHING_SUPPLIED)).toBe(true);
  });

  it("wants the count and nothing else", () => {
    expect(satisfied(COUNT, { ...NOTHING_SUPPLIED, typed: "31" })).toBe(true);
    expect(satisfied(COUNT, { ...NOTHING_SUPPLIED, typed: " 31 " })).toBe(true);
    expect(satisfied(COUNT, NOTHING_SUPPLIED)).toBe(false);
    expect(satisfied(COUNT, { ...NOTHING_SUPPLIED, typed: "30" })).toBe(false);
  });

  it("does not accept something that merely starts with the count", () => {
    // Not `parseInt`, which reads "31 repos" as 31. The whole purpose of this
    // check is that it cannot be satisfied without reading the plan, and a
    // lenient parser gives that away.
    for (const typed of ["31 repos", "31.0", "0x1f", "31\n32", "+31", "3 1"]) {
      expect(satisfied(COUNT, { ...NOTHING_SUPPLIED, typed })).toBe(false);
    }
  });

  it("wants the acknowledgement ticked", () => {
    expect(satisfied(ACK, { ...NOTHING_SUPPLIED, acknowledged: true })).toBe(true);
    expect(satisfied(ACK, NOTHING_SUPPLIED)).toBe(false);
    // ...and typing a number does not stand in for reading a sentence.
    expect(satisfied(ACK, { typed: "31", acknowledged: false })).toBe(false);
  });

  it("does not let one guard's input satisfy another's", () => {
    expect(satisfied(COUNT, { typed: "", acknowledged: true })).toBe(false);
  });
});

describe("isDangerous", () => {
  it("is exactly the presence of a guard", () => {
    // One signal, so nothing can disagree with it: a plan cannot be styled as
    // dangerous while being confirmable by pressing Return.
    expect(isDangerous(null)).toBe(false);
    expect(isDangerous(COUNT)).toBe(true);
    expect(isDangerous(ACK)).toBe(true);
  });
});
