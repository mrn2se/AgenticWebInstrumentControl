import { describe, expect, it } from "vitest";
import { cursorReadout } from "./cursors";

describe("cursorReadout", () => {
  it("computes the absolute time delta regardless of cursor order", () => {
    expect(cursorReadout({ a: 1, b: 1.5 }).deltaSeconds).toBeCloseTo(0.5);
    expect(cursorReadout({ a: 1.5, b: 1 }).deltaSeconds).toBeCloseTo(0.5);
  });

  it("computes 1/deltaT as the implied frequency", () => {
    expect(cursorReadout({ a: 0, b: 0.001 }).frequencyHz).toBeCloseTo(1000);
  });

  it("reports null frequency, not Infinity, when both cursors coincide", () => {
    const readout = cursorReadout({ a: 2, b: 2 });
    expect(readout.deltaSeconds).toBe(0);
    expect(readout.frequencyHz).toBeNull();
  });
});
