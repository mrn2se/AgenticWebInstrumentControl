import { describe, expect, it } from "vitest";
import type { RecChannel } from "../../proto/types";
import type { DecodedViewFrame, ViewFrameHeader } from "../../ws/view-frame";
import { viewFrameToTraces, tracesTimeBounds } from "./view-traces";

function header(overrides: Partial<ViewFrameHeader>): ViewFrameHeader {
  return {
    viewId: 1,
    sequence: 1,
    kind: "digital-runs",
    channelCount: 1,
    sampleBits: 8,
    flags: { follow: true, truncated: false },
    itemCount: 0,
    firstSample: 0n,
    samplesPerItem: 1n,
    ...overrides,
  };
}

function frameFrom(hdr: ViewFrameHeader, bytes: readonly number[]): DecodedViewFrame {
  return { header: hdr, payload: new DataView(new Uint8Array(bytes).buffer) };
}

function digitalChannel(channel: number): RecChannel {
  return { channel, name: `D${channel}`, calibration: { channel, scale: 1, offset: 0, unit: "" } };
}

function analogChannel(channel: number, scale = 1, offset = 0): RecChannel {
  return { channel, name: `CH${channel + 1}`, calibration: { channel, scale, offset, unit: "V" } };
}

/** Encodes an unsigned LEB128 value the way `awic-record::varint::write_uvarint` does (see `view-decode.test.ts`'s own copy of this helper). */
function leb128(v: number): number[] {
  const bytes: number[] = [];
  let value = v;
  for (;;) {
    const byte = value & 0x7f;
    value = Math.floor(value / 128);
    if (value === 0) {
      bytes.push(byte);
      return bytes;
    }
    bytes.push(byte | 0x80);
  }
}

describe("viewFrameToTraces: digital-runs", () => {
  it("turns exact runs into per-channel segments, merging runs where this channel's own bit is unchanged", () => {
    // value 0b01 for 10 samples, then 0b11 for 5 samples, then 0b10 for 5 samples.
    // D0 (bit 0): high for the first 10, then high for 5 more (merges), then low for 5 -> 2 segments.
    // D1 (bit 1): low for 10, then high for 10 (merges across the value change) -> 2 segments.
    const hdr = header({ kind: "digital-runs", channelCount: 2, itemCount: 3, firstSample: 0n });
    const frame = frameFrom(hdr, [0b01, 10, 0b11, 5, 0b10, 5]);
    const traces = viewFrameToTraces(frame, { rateHz: 10, channels: [digitalChannel(0), digitalChannel(1)] });
    expect(traces).toHaveLength(2);

    const d0 = traces.find((t) => t.kind === "digital" && t.channel === 0);
    expect(d0?.kind).toBe("digital");
    if (d0?.kind !== "digital") throw new Error("expected digital trace");
    expect(d0.segments).toEqual([
      { t0: 0, t1: 1.5, state: "high" },
      { t0: 1.5, t1: 2, state: "low" },
    ]);

    const d1 = traces.find((t) => t.kind === "digital" && t.channel === 1);
    if (d1?.kind !== "digital") throw new Error("expected digital trace");
    expect(d1.segments).toEqual([
      { t0: 0, t1: 1, state: "low" },
      { t0: 1, t1: 2, state: "high" },
    ]);
  });

  it("offsets segment times by the frame's firstSample", () => {
    const hdr = header({ kind: "digital-runs", channelCount: 1, itemCount: 1, firstSample: 100n });
    const frame = frameFrom(hdr, [0x01, 10]);
    const traces = viewFrameToTraces(frame, { rateHz: 10, channels: [digitalChannel(0)] });
    if (traces[0]?.kind !== "digital") throw new Error("expected digital trace");
    expect(traces[0].segments).toEqual([{ t0: 10, t1: 11, state: "high" }]);
  });
});

describe("viewFrameToTraces: digital-summary", () => {
  it("maps AND/OR agreement to high/low and disagreement to toggled", () => {
    const hdr = header({
      kind: "digital-summary",
      channelCount: 1,
      itemCount: 3,
      firstSample: 0n,
      samplesPerItem: 4n,
    });
    // item0: and=0 or=0 (low) ; item1: and=1 or=1 (high) ; item2: and=0 or=1 (toggled)
    const frame = frameFrom(hdr, [0, 0, 1, 1, 0, 1]);
    const traces = viewFrameToTraces(frame, { rateHz: 4, channels: [digitalChannel(0)] });
    if (traces[0]?.kind !== "digital") throw new Error("expected digital trace");
    expect(traces[0].segments).toEqual([
      { t0: 0, t1: 1, state: "low" },
      { t0: 1, t1: 2, state: "high" },
      { t0: 2, t1: 3, state: "toggled" },
    ]);
  });
});

describe("viewFrameToTraces: analog-raw", () => {
  it("converts raw codes to volts via the channel's calibration and times them from firstSample", () => {
    const hdr = header({ kind: "analog-raw", channelCount: 1, itemCount: 3, sampleBits: 8, firstSample: 10n });
    const frame = frameFrom(hdr, [10, 20, 30]);
    const traces = viewFrameToTraces(frame, { rateHz: 100, channels: [analogChannel(0, 0.1, -1)] });
    expect(traces).toHaveLength(1);
    if (traces[0]?.kind !== "analog") throw new Error("expected analog trace");
    traces[0].t.forEach((t, i) => expect(t).toBeCloseTo([0.1, 0.11, 0.12][i]!, 10));
    expect(traces[0].v).toEqual([0, 1, 2]);
    expect(traces[0].label).toBe("CH1");
  });
});

describe("tracesTimeBounds", () => {
  it("is null for an empty trace set", () => {
    expect(tracesTimeBounds([])).toBeNull();
  });

  it("spans a digital-runs trace's actual segments, not item_count*samples_per_item", () => {
    // The bug this guards against: a real 1 kHz square wave at 1 MHz over a
    // 20 ms follow window produced ~41 *runs*, and item_count*samples_per_item
    // (samples_per_item is fixed at 1 for runs, SPEC §3.4) computed a span of
    // 41 samples -- 41 microseconds -- instead of the traces' true ~20 ms,
    // collapsing the viewport until nothing was visible on real hardware.
    const hdr = header({ kind: "digital-runs", channelCount: 1, itemCount: 3, firstSample: 0n });
    const frame = frameFrom(hdr, [0b1, ...leb128(500), 0b0, ...leb128(500), 0b1, ...leb128(19000)]);
    const traces = viewFrameToTraces(frame, { rateHz: 1_000_000, channels: [digitalChannel(0)] });
    const bounds = tracesTimeBounds(traces);
    expect(bounds).toEqual({ t0: 0, t1: 20000 / 1_000_000 });
  });

  it("spans an analog trace's t array", () => {
    const hdr = header({ kind: "analog-raw", channelCount: 1, itemCount: 3, sampleBits: 8, firstSample: 0n });
    const frame = frameFrom(hdr, [1, 2, 3]);
    const traces = viewFrameToTraces(frame, { rateHz: 100, channels: [analogChannel(0)] });
    expect(tracesTimeBounds(traces)).toEqual({ t0: 0, t1: 0.02 });
  });

  it("spans an analog-envelope trace's t0/t1 arrays", () => {
    const hdr = header({
      kind: "analog-summary",
      channelCount: 1,
      itemCount: 2,
      sampleBits: 8,
      firstSample: 0n,
      samplesPerItem: 8n,
    });
    const frame = frameFrom(hdr, [1, 2, 3, 4]);
    const traces = viewFrameToTraces(frame, { rateHz: 8, channels: [analogChannel(0)] });
    expect(tracesTimeBounds(traces)).toEqual({ t0: 0, t1: 2 });
  });
});

describe("viewFrameToTraces: analog-summary", () => {
  it("converts min/max codes to a volts envelope timed by samplesPerItem", () => {
    const hdr = header({
      kind: "analog-summary",
      channelCount: 1,
      itemCount: 2,
      sampleBits: 8,
      firstSample: 0n,
      samplesPerItem: 8n,
    });
    const frame = frameFrom(hdr, [100, 150, 90, 160]);
    const traces = viewFrameToTraces(frame, { rateHz: 8, channels: [analogChannel(0, 1, -128)] });
    if (traces[0]?.kind !== "analog-envelope") throw new Error("expected analog-envelope trace");
    expect(traces[0].t0).toEqual([0, 1]);
    expect(traces[0].t1).toEqual([1, 2]);
    expect(traces[0].vMin).toEqual([-28, -38]);
    expect(traces[0].vMax).toEqual([22, 32]);
  });
});
