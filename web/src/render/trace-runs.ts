import type { AnalogEnvelopeTrace, AnalogTrace } from "./types";

/**
 * One analog trace can hold holes. A capture's frames are joined per kind
 * (`TraceAccumulator`), and under backpressure the server alternates raw and
 * min/max frames (SPEC §3.2), so the raw trace misses every chunk sent as
 * min/max and the other way round; a dropped frame leaves a hole in both. Drawn
 * as one line, a hole became a long straight stroke across the signal. These
 * split a trace at its holes, so that each part can be drawn on its own.
 */

/** Where each contiguous part of `count` items starts, given whether item `i` continues from `i - 1`. */
function runStarts(count: number, continues: (i: number) => boolean): number[] {
  const starts = count > 0 ? [0] : [];
  for (let i = 1; i < count; i++) if (!continues(i)) starts.push(i);
  return starts;
}

function sliceRuns<T>(starts: readonly number[], count: number, slice: (from: number, to: number) => T): T[] {
  return starts.map((from, n) => slice(from, starts[n + 1] ?? count));
}

/**
 * Splits where two samples lie further apart than the sampling interval. That
 * interval is the smallest step, and half of it again leaves room for rounding.
 */
export function splitAnalogTrace(trace: AnalogTrace): AnalogTrace[] {
  const { t, v } = trace;
  let interval = Infinity;
  for (let i = 1; i < t.length; i++) {
    const step = t[i]! - t[i - 1]!;
    if (step > 0 && step < interval) interval = step;
  }
  const starts = runStarts(t.length, (i) => t[i]! - t[i - 1]! <= interval * 1.5);
  if (starts.length <= 1) return [trace];
  return sliceRuns(starts, t.length, (from, to) => ({ ...trace, t: t.slice(from, to), v: v.slice(from, to) }));
}

/** Splits where a bucket does not start at the end of the one before, by more than half a bucket. */
export function splitEnvelopeTrace(trace: AnalogEnvelopeTrace): AnalogEnvelopeTrace[] {
  const { t0, t1, vMin, vMax } = trace;
  const starts = runStarts(t0.length, (i) => {
    const width = Math.max(t1[i]! - t0[i]!, t1[i - 1]! - t0[i - 1]!);
    return Math.abs(t0[i]! - t1[i - 1]!) <= width / 2;
  });
  if (starts.length <= 1) return [trace];
  return sliceRuns(starts, t0.length, (from, to) => ({
    ...trace,
    t0: t0.slice(from, to),
    t1: t1.slice(from, to),
    vMin: vMin.slice(from, to),
    vMax: vMax.slice(from, to),
  }));
}
