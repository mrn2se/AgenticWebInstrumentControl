import { describe, expect, it } from "vitest";
import { SequenceTracker } from "./sequence";

describe("SequenceTracker", () => {
  it("returns null for the first frame observed on a stream", () => {
    const tracker = new SequenceTracker();
    expect(tracker.observe(1, 0)).toBeNull();
  });

  it("returns null for contiguous sequences", () => {
    const tracker = new SequenceTracker();
    tracker.observe(1, 5);
    expect(tracker.observe(1, 6)).toBeNull();
  });

  it("detects a gap and reports how many frames were dropped", () => {
    const tracker = new SequenceTracker();
    tracker.observe(1, 5);
    const gap = tracker.observe(1, 8);
    expect(gap).toEqual({ streamId: 1, expectedSequence: 6, actualSequence: 8, droppedCount: 2 });
  });

  it("tracks multiple streams independently", () => {
    const tracker = new SequenceTracker();
    tracker.observe(1, 5);
    tracker.observe(2, 100);
    expect(tracker.observe(1, 6)).toBeNull();
    expect(tracker.observe(2, 101)).toBeNull();
  });

  it("flags an out-of-order or duplicate sequence without a negative dropped count", () => {
    const tracker = new SequenceTracker();
    tracker.observe(1, 10);
    const gap = tracker.observe(1, 9);
    expect(gap).toEqual({ streamId: 1, expectedSequence: 11, actualSequence: 9, droppedCount: 0 });
  });

  it("treats u32 wraparound as contiguous, not a gap", () => {
    const tracker = new SequenceTracker();
    tracker.observe(1, 0xffffffff);
    expect(tracker.observe(1, 0)).toBeNull();
  });

  it("forgets a stream on reset", () => {
    const tracker = new SequenceTracker();
    tracker.observe(1, 5);
    tracker.reset(1);
    expect(tracker.observe(1, 99)).toBeNull();
  });
});
