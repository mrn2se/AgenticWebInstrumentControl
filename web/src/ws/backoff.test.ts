import { describe, expect, it } from "vitest";
import { DEFAULT_BACKOFF, computeBackoffDelay } from "./backoff";

describe("computeBackoffDelay", () => {
  it("grows exponentially up to the configured max, with zero jitter", () => {
    const opts = { initialMs: 100, maxMs: 800, factor: 2, jitterRatio: 0 };
    expect(computeBackoffDelay(0, opts)).toBe(100);
    expect(computeBackoffDelay(1, opts)).toBe(200);
    expect(computeBackoffDelay(2, opts)).toBe(400);
    expect(computeBackoffDelay(3, opts)).toBe(800);
    expect(computeBackoffDelay(10, opts)).toBe(800); // capped
  });

  it("applies bounded jitter around the raw delay", () => {
    const opts = { initialMs: 1000, maxMs: 10_000, factor: 2, jitterRatio: 0.5 };
    const high = computeBackoffDelay(0, opts, () => 1);
    const low = computeBackoffDelay(0, opts, () => 0);
    expect(low).toBeLessThan(1000);
    expect(high).toBeGreaterThan(1000);
    expect(high - 1000).toBeLessThanOrEqual(500);
    expect(1000 - low).toBeLessThanOrEqual(500);
  });

  it("never returns a negative delay even with large jitter ratios", () => {
    const opts = { initialMs: 10, maxMs: 100, factor: 2, jitterRatio: 5 };
    expect(computeBackoffDelay(0, opts, () => 0)).toBeGreaterThanOrEqual(0);
  });

  it("exposes sane defaults", () => {
    expect(DEFAULT_BACKOFF.initialMs).toBeGreaterThan(0);
    expect(DEFAULT_BACKOFF.maxMs).toBeGreaterThan(DEFAULT_BACKOFF.initialMs);
  });
});
