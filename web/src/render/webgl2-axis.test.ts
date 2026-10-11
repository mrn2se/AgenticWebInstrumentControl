import { describe, expect, it } from "vitest";
import { formatSiTime, formatVolts, niceStep, timeTicks, voltTicks } from "./webgl2-axis";

describe("niceStep", () => {
  it.each([
    [3, 5],
    [7, 10],
    [0.03, 0.05],
    [120, 200],
    [1, 1],
    [2, 2],
    [5, 5],
  ])("rounds %f up to the nice step %f", (rough, expected) => {
    expect(niceStep(rough)).toBeCloseTo(expected, 10);
  });

  it("returns 0 for a non-positive input", () => {
    expect(niceStep(0)).toBe(0);
    expect(niceStep(-5)).toBe(0);
  });
});

describe("timeTicks", () => {
  it("produces evenly, nicely spaced ticks covering the span", () => {
    const ticks = timeTicks(0, 1, 5);
    expect(ticks.length).toBeGreaterThan(0);
    for (const t of ticks) {
      expect(t).toBeGreaterThanOrEqual(0);
      expect(t).toBeLessThanOrEqual(1);
    }
    // Consecutive ticks are equally spaced.
    const step = ticks[1]! - ticks[0]!;
    for (let i = 1; i < ticks.length; i++) {
      expect(ticks[i]! - ticks[i - 1]!).toBeCloseTo(step, 10);
    }
  });

  it("returns an empty array for a degenerate or inverted range", () => {
    expect(timeTicks(1, 1)).toEqual([]);
    expect(timeTicks(1, 0)).toEqual([]);
  });
});

describe("formatSiTime", () => {
  it("picks seconds for a step >= 1 s", () => {
    expect(formatSiTime(2, 1)).toBe("2 s");
  });

  it("picks milliseconds for a sub-second step", () => {
    expect(formatSiTime(0.002, 0.001)).toBe("2 ms");
  });

  it("picks microseconds for a sub-millisecond step", () => {
    expect(formatSiTime(0.000_003, 0.000_001)).toBe("3 µs");
  });

  it("picks nanoseconds for a sub-microsecond step", () => {
    expect(formatSiTime(0.000_000_005, 0.000_000_001)).toBe("5 ns");
  });

  it("shows enough decimals to resolve the step", () => {
    // Step of 0.5 ms needs one decimal at ms scale.
    expect(formatSiTime(0.0015, 0.0005)).toBe("1.5 ms");
  });

  it("does not let a value of exactly 0 fall back to a different unit than its neighbors", () => {
    // With a 1 ms step, 0 must still be reported in ms, not ns.
    expect(formatSiTime(0, 0.001)).toBe("0 ms");
  });
});

describe("voltTicks / formatVolts", () => {
  it("produces nicely spaced volt ticks", () => {
    const ticks = voltTicks(-5, 5, 4);
    expect(ticks.length).toBeGreaterThan(0);
    expect(Math.min(...ticks)).toBeGreaterThanOrEqual(-5);
    expect(Math.max(...ticks)).toBeLessThanOrEqual(5);
  });

  it("formats with precision derived from the step", () => {
    expect(formatVolts(1, 1)).toBe("1 V");
    expect(formatVolts(1.5, 0.1)).toBe("1.5 V");
  });
});
