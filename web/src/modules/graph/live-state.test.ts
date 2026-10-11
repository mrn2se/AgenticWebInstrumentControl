import { describe, expect, it, vi } from "vitest";
import {
  columnsForWidth,
  debounce,
  followWindowSeconds,
  initialLiveViewState,
  pauseAtViewport,
  resumeFollowing,
} from "./live-state";

describe("LiveViewState transitions", () => {
  it("starts following with no fixed range", () => {
    expect(initialLiveViewState()).toEqual({ following: true, t0: null, t1: null });
  });

  it("a viewport report pauses following and records the fixed range", () => {
    expect(pauseAtViewport(1.5, 2.5)).toEqual({ following: false, t0: 1.5, t1: 2.5 });
  });

  it("Follow resumes to the initial (following, no range) state", () => {
    expect(resumeFollowing()).toEqual(initialLiveViewState());
  });
});

describe("followWindowSeconds", () => {
  it("passes through a positive value", () => {
    expect(followWindowSeconds(0.02)).toBe(0.02);
  });

  it.each([0, -1, NaN, Infinity])("falls back to 1s for a non-positive or non-finite input %s", (v) => {
    expect(followWindowSeconds(v)).toBe(1);
  });
});

describe("columnsForWidth", () => {
  it("rounds a fractional pixel width", () => {
    expect(columnsForWidth(1599.6)).toBe(1600);
  });

  it("clamps to at least 1 column for a zero or negative width", () => {
    expect(columnsForWidth(0)).toBe(1);
    expect(columnsForWidth(-10)).toBe(1);
  });
});

describe("debounce", () => {
  it("delivers only the last argument after a burst of calls", () => {
    vi.useFakeTimers();
    try {
      const received: number[] = [];
      const d = debounce<number>(100, (v) => received.push(v));
      d.call(1);
      vi.advanceTimersByTime(50);
      d.call(2);
      vi.advanceTimersByTime(50);
      d.call(3);
      expect(received).toEqual([]); // nothing delivered yet — still within the debounce window
      vi.advanceTimersByTime(100);
      expect(received).toEqual([3]);
    } finally {
      vi.useRealTimers();
    }
  });

  it("delivers nothing once cancelled", () => {
    vi.useFakeTimers();
    try {
      const received: number[] = [];
      const d = debounce<number>(100, (v) => received.push(v));
      d.call(1);
      d.cancel();
      vi.advanceTimersByTime(200);
      expect(received).toEqual([]);
    } finally {
      vi.useRealTimers();
    }
  });

  it("delivers independently across separate quiet periods", () => {
    vi.useFakeTimers();
    try {
      const received: number[] = [];
      const d = debounce<number>(100, (v) => received.push(v));
      d.call(1);
      vi.advanceTimersByTime(100);
      expect(received).toEqual([1]);
      d.call(2);
      vi.advanceTimersByTime(100);
      expect(received).toEqual([1, 2]);
    } finally {
      vi.useRealTimers();
    }
  });
});
