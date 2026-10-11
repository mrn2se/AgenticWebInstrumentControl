import { describe, expect, it } from "vitest";
import { type PopoverAction, isEscapeKey, isOutsideAll, nextOpenState } from "./popover";

describe("nextOpenState", () => {
  it("toggle flips the current state either way", () => {
    expect(nextOpenState(false, { type: "toggle" })).toBe(true);
    expect(nextOpenState(true, { type: "toggle" })).toBe(false);
  });

  it("open always yields true", () => {
    expect(nextOpenState(false, { type: "open" })).toBe(true);
    expect(nextOpenState(true, { type: "open" })).toBe(true);
  });

  it.each(["close", "escape", "outside-click"] as const)("%s always yields false", (type) => {
    const action = { type } as PopoverAction;
    expect(nextOpenState(true, action)).toBe(false);
    expect(nextOpenState(false, action)).toBe(false);
  });
});

describe("isEscapeKey", () => {
  it("matches only the literal Escape key name", () => {
    expect(isEscapeKey("Escape")).toBe(true);
    expect(isEscapeKey("Esc")).toBe(false);
    expect(isEscapeKey("a")).toBe(false);
  });
});

describe("isOutsideAll", () => {
  function fake(containsResult: boolean) {
    return { contains: () => containsResult };
  }

  it("is true when every container reports not containing the target", () => {
    expect(isOutsideAll("target", [fake(false), fake(false)])).toBe(true);
  });

  it("is false when any one container contains the target", () => {
    expect(isOutsideAll("target", [fake(false), fake(true)])).toBe(false);
  });

  it("is true (vacuously) for an empty container list", () => {
    expect(isOutsideAll("target", [])).toBe(true);
  });
});
