import { describe, expect, it } from "vitest";
import {
  buildAnalogEnvelopeOutlineVertices,
  buildAnalogEnvelopeVertices,
  buildAnalogLineVertices,
  buildDigitalLevelVertices,
  buildDigitalToggleVertices,
} from "./webgl2-geometry";
import type { AnalogEnvelopeTrace, AnalogTrace, DigitalTrace } from "./types";

describe("buildAnalogLineVertices", () => {
  it("interleaves (t - origin, v) as a line strip, one vertex per sample", () => {
    const trace: AnalogTrace = { kind: "analog", channel: 0, label: "CH1", t: [0, 1, 2], v: [0.1, 0.2, 0.3] };
    const v = buildAnalogLineVertices(trace, 0);
    expect(Array.from(v)).toEqual([0, Math.fround(0.1), 1, Math.fround(0.2), 2, Math.fround(0.3)]);
  });

  it("subtracts the origin so a large absolute time stays small in float32", () => {
    const trace: AnalogTrace = { kind: "analog", channel: 0, label: "CH1", t: [1000, 1000.000001], v: [1, 2] };
    const v = buildAnalogLineVertices(trace, 1000);
    // Without the subtraction, 1000.000001 is not representable distinctly in float32.
    expect(v[0]).toBe(0);
    expect(v[2]).toBeCloseTo(0.000001, 9);
    expect(v[2]).not.toBe(0);
  });

  it("is empty for an empty trace", () => {
    const trace: AnalogTrace = { kind: "analog", channel: 0, label: "CH1", t: [], v: [] };
    expect(buildAnalogLineVertices(trace, 0)).toHaveLength(0);
  });
});

describe("buildAnalogEnvelopeVertices", () => {
  it("emits a continuous triangle strip, 4 vertices per bucket (min,max at each boundary)", () => {
    const trace: AnalogEnvelopeTrace = {
      kind: "analog-envelope",
      channel: 0,
      label: "CH1",
      t0: [0, 1],
      t1: [1, 2],
      vMin: [-1, -2],
      vMax: [1, 2],
    };
    const v = buildAnalogEnvelopeVertices(trace, 0);
    expect(v).toHaveLength(2 * 4 * 2);
    // Every y value in the first bucket's vertices is either its vMin or vMax.
    const firstBucketYs = [v[1], v[3], v[5], v[7]];
    for (const y of firstBucketYs) expect([-1, 1]).toContain(y);
  });

  it("shares the exact boundary x between adjacent buckets — no gap, no interpolation", () => {
    const trace: AnalogEnvelopeTrace = {
      kind: "analog-envelope",
      channel: 0,
      label: "CH1",
      t0: [0, 1, 2],
      t1: [1, 2, 3],
      vMin: [0, 0.1, 0.2],
      vMax: [1, 1.1, 1.2],
    };
    const v = buildAnalogEnvelopeVertices(trace, 0);
    const xs = [...v].filter((_, i) => i % 2 === 0);
    // Only the buckets' own boundary x values appear (0,1,2,3) — nothing
    // interpolated, and the strip's shared boundary (x=1, x=2) appears from
    // both the closing side of one bucket and the opening side of the next.
    expect(new Set(xs)).toEqual(new Set([0, 1, 2, 3]));
  });

  it("does not bridge a real gap between non-contiguous buckets with a stray triangle", () => {
    // Two buckets whose boundaries do NOT touch: the strip must still only
    // ever place a vertex at each bucket's own t0/t1 — a bridging triangle
    // would need an interpolated x between 1 and 10, which never appears.
    const trace: AnalogEnvelopeTrace = {
      kind: "analog-envelope",
      channel: 0,
      label: "CH1",
      t0: [0, 10],
      t1: [1, 11],
      vMin: [0, 0],
      vMax: [1, 1],
    };
    const v = buildAnalogEnvelopeVertices(trace, 0);
    const xs = [...v].filter((_, i) => i % 2 === 0);
    expect(new Set(xs)).toEqual(new Set([0, 1, 10, 11]));
  });

  it("is empty for an empty trace", () => {
    const trace: AnalogEnvelopeTrace = { kind: "analog-envelope", channel: 0, label: "CH1", t0: [], t1: [], vMin: [], vMax: [] };
    expect(buildAnalogEnvelopeVertices(trace, 0)).toHaveLength(0);
  });
});

describe("buildAnalogEnvelopeOutlineVertices", () => {
  it("traces the max profile (top) and min profile (bottom) as continuous line strips", () => {
    const trace: AnalogEnvelopeTrace = {
      kind: "analog-envelope",
      channel: 0,
      label: "CH1",
      t0: [0, 1],
      t1: [1, 2],
      vMin: [-1, -2],
      vMax: [1, 2],
    };
    const { top, bottom } = buildAnalogEnvelopeOutlineVertices(trace, 0);
    // top: (0,1),(1,1),(1,2),(2,2) — one continuous strip through both buckets' max.
    expect(Array.from(top)).toEqual([0, 1, 1, 1, 1, 2, 2, 2]);
    // bottom: (0,-1),(1,-1),(1,-2),(2,-2) — same shape through min.
    expect(Array.from(bottom)).toEqual([0, -1, 1, -1, 1, -2, 2, -2]);
  });

  it("shares the boundary x between adjacent buckets, so the outline has no gap either", () => {
    const trace: AnalogEnvelopeTrace = {
      kind: "analog-envelope",
      channel: 0,
      label: "CH1",
      t0: [0, 1, 2],
      t1: [1, 2, 3],
      vMin: [0, 0, 0],
      vMax: [1, 1, 1],
    };
    const { top } = buildAnalogEnvelopeOutlineVertices(trace, 0);
    const xs = [...top].filter((_, i) => i % 2 === 0);
    expect(new Set(xs)).toEqual(new Set([0, 1, 2, 3]));
  });
});

function digital(segments: DigitalTrace["segments"]): DigitalTrace {
  return { kind: "digital", channel: 0, label: "D0", lane: 0, segments };
}

describe("buildDigitalLevelVertices", () => {
  it("draws a level line for a single high segment", () => {
    const trace = digital([{ t0: 0, t1: 1, state: "high" }]);
    const v = buildDigitalLevelVertices(trace, 0);
    expect(Array.from(v)).toEqual([0, 1, 1, 1]);
  });

  it("adds a vertical edge between two contiguous segments at different levels", () => {
    const trace = digital([
      { t0: 0, t1: 1, state: "low" },
      { t0: 1, t1: 2, state: "high" },
    ]);
    const v = buildDigitalLevelVertices(trace, 0);
    // low segment, vertical edge at x=1 from y=0 to y=1, then the high segment.
    expect(Array.from(v)).toEqual([0, 0, 1, 0, /* edge */ 1, 0, 1, 1, /* high seg */ 1, 1, 2, 1]);
  });

  it("never draws a level through a toggled segment — the line breaks instead", () => {
    const trace = digital([
      { t0: 0, t1: 1, state: "low" },
      { t0: 1, t1: 2, state: "toggled" },
      { t0: 2, t1: 3, state: "high" },
    ]);
    const v = buildDigitalLevelVertices(trace, 0);
    // No vertex anywhere has an x strictly between 1 and 2 (the toggled span) at all,
    // and no line segment connects x=1 to x=2.
    const points: Array<[number, number]> = [];
    for (let i = 0; i < v.length; i += 2) points.push([v[i]!, v[i + 1]!]);
    const pairs: Array<[[number, number], [number, number]]> = [];
    for (let i = 0; i < points.length; i += 2) pairs.push([points[i]!, points[i + 1]!]);
    for (const [a, b] of pairs) {
      const spansToggled = Math.min(a[0], b[0]) < 2 && Math.max(a[0], b[0]) > 1 && a[0] !== b[0];
      expect(spansToggled).toBe(false);
    }
  });

  it("subtracts the origin from every x", () => {
    const trace = digital([{ t0: 1000, t1: 1001, state: "high" }]);
    const v = buildDigitalLevelVertices(trace, 1000);
    expect(v[0]).toBe(0);
    expect(v[2]).toBe(1);
  });
});

describe("buildDigitalToggleVertices", () => {
  it("emits a full-height filled quad only for toggled segments", () => {
    const trace = digital([
      { t0: 0, t1: 1, state: "low" },
      { t0: 1, t1: 2, state: "toggled" },
    ]);
    const v = buildDigitalToggleVertices(trace, 0);
    expect(v).toHaveLength(6 * 2); // one quad, two triangles
    const ys = [v[1], v[3], v[5], v[7], v[9], v[11]];
    for (const y of ys) expect([0, 1]).toContain(y);
    const xs = [v[0], v[2], v[4], v[6], v[8], v[10]];
    for (const x of xs) expect([1, 2]).toContain(x);
  });

  it("is empty when nothing is toggled", () => {
    const trace = digital([{ t0: 0, t1: 1, state: "high" }]);
    expect(buildDigitalToggleVertices(trace, 0)).toHaveLength(0);
  });

  it("merges a run of contiguous toggled segments into one quad, not one per segment", () => {
    const run = digital([
      { t0: 0, t1: 1, state: "low" },
      { t0: 1, t1: 2, state: "toggled" },
      { t0: 2, t1: 3, state: "toggled" },
      { t0: 3, t1: 4, state: "toggled" },
      { t0: 4, t1: 5, state: "high" },
    ]);
    const v = buildDigitalToggleVertices(run, 0);
    expect(v).toHaveLength(6 * 2); // one merged quad, not three
    const xs = [v[0], v[2], v[4], v[6], v[8], v[10]];
    for (const x of xs) expect([1, 4]).toContain(x); // spans the whole run, 1..4
  });

  it("keeps two toggled runs separated by a non-toggled segment as two independent quads", () => {
    const twoRuns = digital([
      { t0: 0, t1: 1, state: "toggled" },
      { t0: 1, t1: 2, state: "low" },
      { t0: 2, t1: 3, state: "toggled" },
    ]);
    const v = buildDigitalToggleVertices(twoRuns, 0);
    expect(v).toHaveLength(2 * 6 * 2); // two separate quads
    const xs = [...v].filter((_, i) => i % 2 === 0);
    // Only the two runs' own boundaries appear — nothing bridges the low segment in between.
    expect(new Set(xs)).toEqual(new Set([0, 1, 2, 3]));
  });
});
