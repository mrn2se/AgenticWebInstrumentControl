import { describe, expect, it } from "vitest";
import { splitAnalogTrace, splitEnvelopeTrace } from "./trace-runs";
import { frameToTraces, TraceAccumulator, type TraceContext } from "../modules/graph/traces";
import type { DecodedFrame } from "../ws/frame";
import type { AnalogEnvelopeTrace, AnalogTrace } from "./types";

const raw = (t: number[]): AnalogTrace => ({ kind: "analog", channel: 0, label: "CH1", t, v: t.map(() => 0) });
const envelope = (buckets: Array<[number, number]>): AnalogEnvelopeTrace => ({
  kind: "analog-envelope",
  channel: 0,
  label: "CH1",
  t0: buckets.map(([a]) => a),
  t1: buckets.map(([, b]) => b),
  vMin: buckets.map(() => -1),
  vMax: buckets.map(() => 1),
});

describe("splitAnalogTrace", () => {
  it("keeps a contiguous trace whole, the same object", () => {
    const trace = raw([0, 1, 2, 3]);
    expect(splitAnalogTrace(trace)).toEqual([trace]);
    expect(splitAnalogTrace(trace)[0]).toBe(trace);
  });

  it("splits at a hole, with each sample's value staying with it", () => {
    const trace: AnalogTrace = { ...raw([0, 1, 2, 10, 11]), v: [5, 6, 7, 8, 9] };
    const runs = splitAnalogTrace(trace);
    expect(runs.map((r) => r.t)).toEqual([[0, 1, 2], [10, 11]]);
    expect(runs.map((r) => r.v)).toEqual([[5, 6, 7], [8, 9]]);
  });

  it("does not take rounding in the time values for a hole", () => {
    const dt = 1 / 48e6;
    const t = Array.from({ length: 1000 }, (_, i) => 1000 + i * dt);
    expect(splitAnalogTrace(raw(t))).toHaveLength(1);
  });

  it("keeps a single sample, and an empty trace", () => {
    expect(splitAnalogTrace(raw([3]))).toHaveLength(1);
    expect(splitAnalogTrace(raw([]))).toHaveLength(1);
  });
});

describe("splitEnvelopeTrace", () => {
  it("keeps contiguous buckets whole, also where their width changes", () => {
    const trace = envelope([[0, 1], [1, 2], [2, 2.5], [2.5, 3]]);
    expect(splitEnvelopeTrace(trace)[0]).toBe(trace);
  });

  it("splits where a bucket does not start at the end of the one before", () => {
    const runs = splitEnvelopeTrace(envelope([[0, 1], [1, 2], [5, 6], [6, 7]]));
    expect(runs.map((r) => r.t0)).toEqual([[0, 1], [5, 6]]);
    expect(runs.map((r) => r.t1)).toEqual([[1, 2], [6, 7]]);
    expect(runs.map((r) => r.vMax)).toEqual([[1, 1], [1, 1]]);
  });
});

describe("a capture that alternates raw and min/max frames", () => {
  const rateHz = 1000;
  const ctx: TraceContext = {
    timebase: { rateHz, totalSamples: 12, frameSpanSeconds: 0.004 },
    calibrationForChannel: (channel) => ({ channel, scale: 1, offset: 0, unit: "V" }),
    labelForChannel: () => "CH1",
    laneForChannel: () => 0,
  };
  const frame = (encoding: "u8-interleaved" | "minmax-u8", sequence: number, payload: number[]): DecodedFrame =>
    ({
      header: { encoding, sequence, streamId: 1, channelCount: 1, sampleCount: encoding === "u8-interleaved" ? payload.length : payload.length / 2, t0Ns: BigInt(sequence * 4_000_000) },
      payload: new DataView(new Uint8Array(payload).buffer),
    }) as unknown as DecodedFrame;

  it("draws neither kind across the chunks of the other", () => {
    const traces = new TraceAccumulator();
    traces.add(frameToTraces(frame("u8-interleaved", 0, [1, 2, 3, 4]), ctx));
    traces.add(frameToTraces(frame("minmax-u8", 1, [1, 2, 3, 4]), ctx));
    traces.add(frameToTraces(frame("u8-interleaved", 2, [1, 2, 3, 4]), ctx));
    traces.add(frameToTraces(frame("minmax-u8", 3, [1, 2, 3, 4]), ctx));
    const [rawTrace, envelopeTrace] = traces.snapshot() as [AnalogTrace, AnalogEnvelopeTrace];
    expect(splitAnalogTrace(rawTrace).map((r) => r.t.length)).toEqual([4, 4]);
    expect(splitEnvelopeTrace(envelopeTrace).map((r) => r.t0.length)).toEqual([2, 2]);
  });
});
