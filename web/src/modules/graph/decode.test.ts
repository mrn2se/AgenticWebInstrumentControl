import { describe, expect, it } from "vitest";
import { buildSampleFrame } from "./fixtures";
import {
  SampleDecodeError,
  decodeSampleFrame,
  digitalBit,
  splitBitwiseChannels,
  splitDigitalChannels,
} from "./decode";

describe("decodeSampleFrame: u8-interleaved (encoding 0)", () => {
  it("de-interleaves ch0,ch1,ch0,ch1,... into separate per-channel arrays", () => {
    // 2 channels x 3 samples, interleaved: ch0[0],ch1[0],ch0[1],ch1[1],ch0[2],ch1[2]
    const frame = buildSampleFrame({
      encoding: "u8-interleaved",
      channelCount: 2,
      sampleCount: 3,
      payload: [10, 100, 11, 101, 12, 102],
    });
    const data = decodeSampleFrame(frame);
    expect(data).toEqual({
      kind: "analog-raw",
      channels: [
        { channel: 0, codes: [10, 11, 12] },
        { channel: 1, codes: [100, 101, 102] },
      ],
    });
  });

  it("rejects a payload whose length doesn't match channelCount x sampleCount", () => {
    const frame = buildSampleFrame({ encoding: "u8-interleaved", channelCount: 2, sampleCount: 3, payload: [1, 2] });
    expect(() => decodeSampleFrame(frame)).toThrow(SampleDecodeError);
  });
});

describe("decodeSampleFrame: i16 (encoding 1)", () => {
  it("decodes interleaved little-endian signed 16-bit samples, including negatives", () => {
    // ch0[0]=-1 (0xFFFF), ch1[0]=1000 (0x03E8), ch0[1]=-2 (0xFFFE), ch1[1]=-1000
    const frame = buildSampleFrame({
      encoding: "i16",
      channelCount: 2,
      sampleCount: 2,
      payload: [0xff, 0xff, 0xe8, 0x03, 0xfe, 0xff, 0x18, 0xfc],
    });
    const data = decodeSampleFrame(frame);
    expect(data).toEqual({
      kind: "analog-raw",
      channels: [
        { channel: 0, codes: [-1, -2] },
        { channel: 1, codes: [1000, -1000] },
      ],
    });
  });
});

describe("decodeSampleFrame: packed-digital (encoding 2)", () => {
  it("keeps one byte per sample, unsplit — not channel-major, not interleaved per channel", () => {
    const frame = buildSampleFrame({
      encoding: "packed-digital",
      channelCount: 3,
      sampleCount: 4,
      payload: [0b001, 0b010, 0b101, 0b000],
    });
    const data = decodeSampleFrame(frame);
    expect(data).toEqual({ kind: "digital-raw", channelCount: 3, samples: [0b001, 0b010, 0b101, 0b000] });
  });

  it("splits packed bytes into one 0/1 level array per channel bit", () => {
    const frame = buildSampleFrame({
      encoding: "packed-digital",
      channelCount: 3,
      sampleCount: 4,
      payload: [0b001, 0b010, 0b101, 0b000],
    });
    const data = decodeSampleFrame(frame);
    if (data.kind !== "digital-raw") throw new Error("expected digital-raw");
    expect(splitDigitalChannels(data)).toEqual([
      { channel: 0, levels: [1, 0, 1, 0] },
      { channel: 1, levels: [0, 1, 0, 0] },
      { channel: 2, levels: [0, 0, 1, 0] },
    ]);
  });
});

describe("decodeSampleFrame: minmax-u8 (encoding 3) — channel-major", () => {
  it("reads all of channel 0's (min,max) pairs before any of channel 1's", () => {
    // channel-major, 2 channels x 2 buckets: ch0=[(1,9),(2,8)], ch1=[(50,60),(51,61)]
    const frame = buildSampleFrame({
      encoding: "minmax-u8",
      channelCount: 2,
      sampleCount: 2,
      payload: [1, 9, 2, 8, 50, 60, 51, 61],
    });
    const data = decodeSampleFrame(frame);
    expect(data).toEqual({
      kind: "analog-envelope",
      channels: [
        { channel: 0, min: [1, 2], max: [9, 8] },
        { channel: 1, min: [50, 51], max: [60, 61] },
      ],
    });
  });

  it("would misread as interleaved if buckets were channel-major but decoded as interleaved", () => {
    // Sanity check distinguishing the two layouts: an interleaved reading of
    // the same bytes would pair ch0=[(1,9),(50,60)], which is wrong.
    const frame = buildSampleFrame({
      encoding: "minmax-u8",
      channelCount: 2,
      sampleCount: 2,
      payload: [1, 9, 2, 8, 50, 60, 51, 61],
    });
    const data = decodeSampleFrame(frame);
    if (data.kind !== "analog-envelope") throw new Error("expected analog-envelope");
    expect(data.channels[0]?.min).not.toEqual([1, 50]);
  });
});

describe("decodeSampleFrame: minmax-i16 (encoding 4) — channel-major", () => {
  it("decodes channel-major little-endian signed 16-bit min/max pairs", () => {
    // 1 channel x 2 buckets: (-100, 100), (-200, 200)
    const toLE = (n: number): [number, number] => {
      const v = n < 0 ? 0x10000 + n : n;
      return [v & 0xff, (v >> 8) & 0xff];
    };
    const payload = [...toLE(-100), ...toLE(100), ...toLE(-200), ...toLE(200)];
    const frame = buildSampleFrame({ encoding: "minmax-i16", channelCount: 1, sampleCount: 2, payload });
    const data = decodeSampleFrame(frame);
    expect(data).toEqual({
      kind: "analog-envelope",
      channels: [{ channel: 0, min: [-100, -200], max: [100, 200] }],
    });
  });
});

describe("decodeSampleFrame: bitwise-digital (encoding 5) — one stream, never numeric min/max", () => {
  it("extracts the AND/OR byte pair per bucket without splitting by channel", () => {
    const frame = buildSampleFrame({
      encoding: "bitwise-digital",
      channelCount: 8,
      sampleCount: 2,
      payload: [0x00, 0x81, 0xff, 0xff],
    });
    const data = decodeSampleFrame(frame);
    expect(data).toEqual({ kind: "digital-bitwise", channelCount: 8, and: [0x00, 0xff], or: [0x81, 0xff] });
  });

  it("classifies a (AND=0x00, OR=0x81) bucket as toggled for D0 and D7, low for D1-D6", () => {
    const frame = buildSampleFrame({
      encoding: "bitwise-digital",
      channelCount: 8,
      sampleCount: 1,
      payload: [0x00, 0x81],
    });
    const data = decodeSampleFrame(frame);
    if (data.kind !== "digital-bitwise") throw new Error("expected digital-bitwise");
    const channels = splitBitwiseChannels(data);

    expect(channels[0]).toEqual({ channel: 0, states: ["toggled"] }); // D0
    expect(channels[7]).toEqual({ channel: 7, states: ["toggled"] }); // D7
    for (const ch of [1, 2, 3, 4, 5, 6]) {
      expect(channels[ch]).toEqual({ channel: ch, states: ["low"] });
    }
  });

  it("classifies (AND=1,OR=1) as high throughout and (AND=0,OR=0) as low throughout", () => {
    const frame = buildSampleFrame({
      encoding: "bitwise-digital",
      channelCount: 2,
      sampleCount: 1,
      payload: [0b11, 0b11],
    });
    const data = decodeSampleFrame(frame);
    if (data.kind !== "digital-bitwise") throw new Error("expected digital-bitwise");
    expect(splitBitwiseChannels(data)).toEqual([
      { channel: 0, states: ["high"] },
      { channel: 1, states: ["high"] },
    ]);
  });

  it("never reads AND/OR bytes as a numeric min/max pair (the documented failure mode)", () => {
    // D0 high (0x01) and a D7 glitch (0x80) across the bucket: AND=0x00, OR=0x81.
    // A numeric-min/max misreading would report min=0x00,max=0x81 and treat
    // that as "mostly low with one big excursion" rather than two
    // independent single-bit toggles — this asserts the bit-correct result.
    const frame = buildSampleFrame({
      encoding: "bitwise-digital",
      channelCount: 8,
      sampleCount: 1,
      payload: [0x00, 0x81],
    });
    const data = decodeSampleFrame(frame);
    if (data.kind !== "digital-bitwise") throw new Error("expected digital-bitwise");
    const channels = splitBitwiseChannels(data);
    expect(channels.map((c) => c.states[0])).toEqual(["toggled", "low", "low", "low", "low", "low", "low", "toggled"]);
  });
});

describe("digitalBit", () => {
  it.each([
    [0b0000_0001, 0, 1],
    [0b0000_0001, 1, 0],
    [0b1000_0000, 7, 1],
    [0b1000_0000, 0, 0],
  ] as const)("digitalBit(0b%s, %i) === %i", (byte, n, expected) => {
    expect(digitalBit(byte, n)).toBe(expected);
  });
});
