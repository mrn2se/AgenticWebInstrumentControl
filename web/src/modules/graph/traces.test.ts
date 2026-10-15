import { describe, expect, it } from "vitest";
import { buildSampleFrame } from "./fixtures";
import { FrameSpanPlacer, frameToTraces, type PlacedFrame, TraceAccumulator, type TraceContext } from "./traces";
import type { Trace } from "../../render/types";
import type { AnalogTrace, DigitalTrace } from "../../render/types";

// Code 128 is 0 V: an 8-bit, ±5 V channel centred on mid-scale.
const calibrationForChannel = (channel: number) => ({ channel, scale: 10 / 256, offset: -5, unit: "V" });

function ctx(overrides: Partial<TraceContext> = {}): TraceContext {
  return {
    timebase: { rateHz: 1000, totalSamples: 4 },
    calibrationForChannel,
    labelForChannel: (kind, channel) => `${kind}${channel}`,
    laneForChannel: (channel) => channel,
    ...overrides,
  };
}

describe("frameToTraces", () => {
  it("places raw analog samples at 1/rateHz spacing starting at t0", () => {
    const frame = buildSampleFrame({
      encoding: "u8-interleaved",
      channelCount: 1,
      sampleCount: 3,
      payload: [128, 128, 128],
      t0Ns: 2_000_000_000n, // 2 seconds
    });
    const [trace] = frameToTraces(frame, ctx()) as [AnalogTrace];
    expect(trace.kind).toBe("analog");
    expect(trace.t).toEqual([2, 2.001, 2.002]);
    expect(trace.v.every((v) => Math.abs(v) < 1e-9)).toBe(true); // code 128 -> 0V at this scale
  });

  it("spaces decimated envelope buckets across the full configured capture span", () => {
    // totalSamples=4, rateHz=1000 -> capture spans 4ms; 2 buckets -> 2ms each.
    const frame = buildSampleFrame({
      encoding: "minmax-u8",
      channelCount: 1,
      sampleCount: 2,
      payload: [0, 255, 10, 245],
    });
    const [trace] = frameToTraces(frame, ctx());
    if (trace?.kind !== "analog-envelope") throw new Error("expected analog-envelope");
    expect(trace.t0).toEqual([0, 0.002]);
    expect(trace.t1).toEqual([0.002, 0.004]);
  });

  it("run-length encodes raw digital levels into level segments", () => {
    // channel 0's bit is 1,1,0 across 3 samples at 1kHz -> one 2-sample high segment, one low.
    const frame = buildSampleFrame({
      encoding: "packed-digital",
      channelCount: 1,
      sampleCount: 3,
      payload: [0b1, 0b1, 0b0],
    });
    const [trace] = frameToTraces(frame, ctx()) as [DigitalTrace];
    expect(trace.segments).toEqual([
      { t0: 0, t1: 0.002, state: "high" },
      { t0: 0.002, t1: 0.003, state: "low" },
    ]);
  });

  it("emits a toggled segment for a bitwise bucket where AND/OR disagree, never a level", () => {
    const frame = buildSampleFrame({
      encoding: "bitwise-digital",
      channelCount: 1,
      sampleCount: 1,
      payload: [0x00, 0x01],
    });
    const [trace] = frameToTraces(frame, ctx({ timebase: { rateHz: 1000, totalSamples: 1 } })) as [DigitalTrace];
    expect(trace.segments).toEqual([{ t0: 0, t1: 0.001, state: "toggled" }]);
  });

  it("uses the caller-supplied label and lane for each channel", () => {
    const frame = buildSampleFrame({
      encoding: "packed-digital",
      channelCount: 2,
      sampleCount: 1,
      payload: [0b11],
    });
    const traces = frameToTraces(
      frame,
      ctx({ labelForChannel: (_k, ch) => `D${ch}`, laneForChannel: (ch) => 10 + ch }),
    ) as DigitalTrace[];
    expect(traces.map((t) => t.label)).toEqual(["D0", "D1"]);
    expect(traces.map((t) => t.lane)).toEqual([10, 11]);
  });
});

// B-72: a capture larger than one frame arrives as several, and the tab drew
// only the last one.
describe("TraceAccumulator", () => {
  const appendTraces = (previous: readonly Trace[], next: readonly Trace[]): Trace[] => {
    const acc = new TraceAccumulator();
    acc.add(previous);
    acc.add(next);
    return acc.snapshot();
  };

  const analog = (t: number[], v: number[], channel = 0): AnalogTrace => ({ kind: "analog", channel, label: `a${channel}`, t, v });
  const digital = (segments: DigitalTrace["segments"], channel = 0): DigitalTrace => ({
    kind: "digital",
    channel,
    label: `d${channel}`,
    lane: channel,
    segments,
  });

  it("joins a channel's samples across frames in order", () => {
    const [trace] = appendTraces([analog([0, 1], [5, 6])], [analog([2, 3], [7, 8])]) as [AnalogTrace];
    expect(trace.t).toEqual([0, 1, 2, 3]);
    expect(trace.v).toEqual([5, 6, 7, 8]);
  });

  it("keeps channels apart, and adds one that first appears in a later frame", () => {
    const merged = appendTraces([analog([0], [1], 0)], [analog([1], [2], 0), analog([1], [3], 1)]) as AnalogTrace[];
    expect(merged.map((t) => [t.channel, t.t])).toEqual([
      [0, [0, 1]],
      [1, [1]],
    ]);
  });

  it("merges a digital level held across the frame boundary into one segment", () => {
    const [trace] = appendTraces(
      [digital([{ t0: 0, t1: 1, state: "low" }, { t0: 1, t1: 2, state: "high" }])],
      [digital([{ t0: 2, t1: 3, state: "high" }, { t0: 3, t1: 4, state: "low" }])],
    ) as [DigitalTrace];
    expect(trace.segments).toEqual([
      { t0: 0, t1: 1, state: "low" },
      { t0: 1, t1: 3, state: "high" },
      { t0: 3, t1: 4, state: "low" },
    ]);
  });

  it("never merges toggled buckets, which mark lost transitions", () => {
    const [trace] = appendTraces(
      [digital([{ t0: 0, t1: 1, state: "toggled" }])],
      [digital([{ t0: 1, t1: 2, state: "toggled" }])],
    ) as [DigitalTrace];
    expect(trace.segments).toHaveLength(2);
  });

  it("starts from the frame itself when there is nothing before it", () => {
    const next = [analog([0], [1])];
    expect(appendTraces([], next)).toEqual(next);
  });

  it("hands out snapshots that later frames do not change", () => {
    const acc = new TraceAccumulator();
    acc.add([analog([0], [1])]);
    const first = acc.snapshot() as [AnalogTrace];
    acc.add([analog([1], [2])]);
    expect(first[0].t).toEqual([0]);
    expect((acc.snapshot() as [AnalogTrace])[0].t).toEqual([0, 1]);
  });
});

// #48: a 1.28 s capture streamed as 8 ms frames fell back to decimated frames
// under backpressure, and each was drawn the length of the whole capture.
// Scaled down: 1 kHz, 12 samples, 4-sample chunks, decimated to 2 buckets.
describe("FrameSpanPlacer", () => {
  const rateHz = 1000;
  const raw = (sequence: number, t0Ms: number) =>
    buildSampleFrame({ encoding: "packed-digital", channelCount: 1, sampleCount: 4, payload: [1, 1, 1, 1], sequence, t0Ns: BigInt(t0Ms) * 1_000_000n });
  // Bucket 0: D0 high throughout; bucket 1: D0 toggled.
  const decimated = (sequence: number, t0Ms: number) =>
    buildSampleFrame({ encoding: "bitwise-digital", channelCount: 1, sampleCount: 2, payload: [0x01, 0x01, 0x00, 0x01], sequence, t0Ns: BigInt(t0Ms) * 1_000_000n });
  const draw = (placed: readonly PlacedFrame[]) => {
    const acc = new TraceAccumulator();
    for (const { frame, spanSeconds } of placed) {
      acc.add(frameToTraces(frame, ctx({ timebase: { rateHz, totalSamples: 12, frameSpanSeconds: spanSeconds } })));
    }
    return (acc.snapshot() as [DigitalTrace])[0].segments;
  };
  const close = (segments: readonly { t0: number; t1: number; state: string }[]) =>
    segments.map((s) => ({ t0: +s.t0.toFixed(9), t1: +s.t1.toFixed(9), state: s.state }));

  it("spreads each decimated frame over its own chunk, not the whole capture", () => {
    const placer = new FrameSpanPlacer(rateHz);
    const placed = [...placer.push(raw(0, 0)), ...placer.push(decimated(1, 4)), ...placer.push(decimated(2, 8)), ...placer.finish(0.012)];
    expect(placed.map((p) => p.spanSeconds)).toEqual([undefined, 0.004, 0.004]);
    // Ends with the capture at 12 ms; before the fix it ran on to 8 + 12 = 20 ms.
    expect(close(draw(placed))).toEqual([
      { t0: 0, t1: 0.006, state: "high" },
      { t0: 0.006, t1: 0.008, state: "toggled" },
      { t0: 0.008, t1: 0.01, state: "high" },
      { t0: 0.01, t1: 0.012, state: "toggled" },
    ]);
  });

  it("holds a decimated frame until the next one shows where it ends", () => {
    const placer = new FrameSpanPlacer(rateHz);
    expect(placer.push(decimated(0, 0))).toEqual([]);
    const placed = placer.push(raw(1, 4));
    expect(placed.map((p) => [p.frame.header.sequence, p.spanSeconds])).toEqual([[0, 0.004], [1, undefined]]);
  });

  it("does not stretch a decimated frame over a dropped one", () => {
    const placer = new FrameSpanPlacer(rateHz);
    placer.push(raw(0, 0));
    placer.push(decimated(1, 4));
    // Sequence 2 (8 ms) was dropped by the server: frame 1 still covers one 4 ms chunk.
    expect(placer.push(decimated(3, 12)).map((p) => p.spanSeconds)).toEqual([0.004]);
  });

  it("still lets a capture sent as one decimated frame tile the whole capture", () => {
    const placer = new FrameSpanPlacer(rateHz);
    expect(placer.push(decimated(0, 0))).toEqual([]);
    expect(placer.finish(0.012).map((p) => p.spanSeconds)).toEqual([0.012]);
  });

  it("places a decimated frame arriving after the end at once, within the end", () => {
    const placer = new FrameSpanPlacer(rateHz);
    placer.push(raw(0, 0));
    expect(placer.finish(0.008)).toEqual([]);
    expect(placer.push(decimated(1, 4)).map((p) => p.spanSeconds)).toEqual([0.004]);
  });
});

// #45: with only D5 and D7 enabled the analyzer's frame says channel_count 2,
// but the levels stay on bits 5 and 7 (SPEC §3.2: bit n = channel Dn).
describe("frameToTraces with a subset of digital channels", () => {
  const lanes = [5, 7];
  const subset = ctx({ digitalChannels: lanes, laneForChannel: (channel) => lanes.indexOf(channel) });

  it("reads each enabled channel from its own bit, not from bits 0..channelCount-1", () => {
    // D5 high then low, D7 low then high; bits 0 and 1 stay 0.
    const frame = buildSampleFrame({ encoding: "packed-digital", channelCount: 2, sampleCount: 2, payload: [1 << 5, 1 << 7] });
    const traces = frameToTraces(frame, subset) as DigitalTrace[];
    expect(traces.map((t) => [t.channel, t.lane])).toEqual([[5, 0], [7, 1]]);
    expect(traces[0]!.segments.map((s) => s.state)).toEqual(["high", "low"]);
    expect(traces[1]!.segments.map((s) => s.state)).toEqual(["low", "high"]);
  });

  it("does the same for a decimated bitwise frame", () => {
    // One bucket: D5 high throughout (AND=OR=1), D7 toggled (AND 0, OR 1).
    const frame = buildSampleFrame({ encoding: "bitwise-digital", channelCount: 2, sampleCount: 1, payload: [1 << 5, (1 << 5) | (1 << 7)] });
    const traces = frameToTraces(frame, subset) as DigitalTrace[];
    expect(traces.map((t) => [t.channel, t.segments[0]!.state])).toEqual([[5, "high"], [7, "toggled"]]);
  });
});
