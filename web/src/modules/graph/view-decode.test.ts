import { describe, expect, it } from "vitest";
import {
  ViewDecodeError,
  decodeAnalogRaw,
  decodeAnalogSummary,
  decodeDigitalRuns,
  decodeDigitalSummary,
} from "./view-decode";

/** Encodes an unsigned LEB128 value the same way `awic-record::varint::write_uvarint` does, for building test payloads. */
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

function dataViewFrom(bytes: readonly number[]): DataView {
  const buf = new Uint8Array(bytes).buffer;
  return new DataView(buf);
}

describe("decodeDigitalRuns", () => {
  it("decodes a sequence of (value, length) runs with single-byte lengths", () => {
    const bytes = [0x01, 10, 0x00, 5, 0xff, 1];
    const runs = decodeDigitalRuns(dataViewFrom(bytes), 3);
    expect(runs).toEqual([
      { value: 0x01, length: 10 },
      { value: 0x00, length: 5 },
      { value: 0xff, length: 1 },
    ]);
  });

  it("decodes a multi-byte LEB128 length (300 samples, per SPEC's own varint test vector)", () => {
    const bytes = [0x05, ...leb128(300)];
    const runs = decodeDigitalRuns(dataViewFrom(bytes), 1);
    expect(runs).toEqual([{ value: 0x05, length: 300 }]);
    expect(bytes.slice(1)).toEqual([0xac, 0x02]); // matches awic-record's known_values test
  });

  it("decodes a very large length without 32-bit wraparound", () => {
    const big = 5_000_000_000; // > 2^32
    const bytes = [0x01, ...leb128(big)];
    const runs = decodeDigitalRuns(dataViewFrom(bytes), 1);
    expect(runs[0]?.length).toBe(big);
  });

  it("throws when the payload runs out of bytes before item_count runs are read", () => {
    expect(() => decodeDigitalRuns(dataViewFrom([0x01, 10]), 2)).toThrow(/expected 2 runs/);
  });

  it("throws on a truncated LEB128 continuation", () => {
    expect(() => decodeDigitalRuns(dataViewFrom([0x01, 0x80]), 1)).toThrow(/truncated LEB128/);
  });
});

describe("decodeDigitalSummary", () => {
  it("decodes AND/OR pairs in order", () => {
    const items = decodeDigitalSummary(dataViewFrom([0x00, 0x00, 0x0f, 0x0f, 0x00, 0xff]), 3);
    expect(items).toEqual([
      { and: 0x00, or: 0x00 },
      { and: 0x0f, or: 0x0f },
      { and: 0x00, or: 0xff },
    ]);
  });

  it("throws on a short payload", () => {
    expect(() => decodeDigitalSummary(dataViewFrom([0x00]), 1)).toThrow(/expected 2 bytes/);
  });
});

describe("decodeAnalogRaw", () => {
  it("decodes 8-bit interleaved codes for 2 channels", () => {
    // ch0[0]=10 ch1[0]=20 ch0[1]=11 ch1[1]=21 ch0[2]=12 ch1[2]=22
    const channels = decodeAnalogRaw(dataViewFrom([10, 20, 11, 21, 12, 22]), 2, 3, 8);
    expect(channels).toEqual([
      { channel: 0, codes: [10, 11, 12] },
      { channel: 1, codes: [20, 21, 22] },
    ]);
  });

  it("decodes 16-bit interleaved signed codes for 1 channel", () => {
    const buf = new ArrayBuffer(4);
    const view = new DataView(buf);
    view.setInt16(0, -100, true);
    view.setInt16(2, 200, true);
    const channels = decodeAnalogRaw(view, 1, 2, 16);
    expect(channels).toEqual([{ channel: 0, codes: [-100, 200] }]);
  });

  it("throws when the payload is shorter than the header promises", () => {
    expect(() => decodeAnalogRaw(dataViewFrom([1, 2]), 2, 2, 8)).toThrow(/expected 4 bytes/);
  });
});

describe("decodeAnalogSummary", () => {
  it("decodes channel-major (min,max) pairs for 2 channels", () => {
    // channel 0: (1,9),(2,8) ; channel 1: (3,7),(4,6)
    const channels = decodeAnalogSummary(dataViewFrom([1, 9, 2, 8, 3, 7, 4, 6]), 2, 2, 8);
    expect(channels).toEqual([
      { channel: 0, min: [1, 2], max: [9, 8] },
      { channel: 1, min: [3, 4], max: [7, 6] },
    ]);
  });

  it("throws when the payload is shorter than the header promises", () => {
    expect(() => decodeAnalogSummary(dataViewFrom([1, 2]), 1, 2, 8)).toThrow(ViewDecodeError);
  });
});
