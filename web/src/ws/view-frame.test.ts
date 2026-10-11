import { describe, expect, it } from "vitest";
import {
  VIEW_FRAME_HEADER_LEN,
  ViewFrameDecodeError,
  decodeViewFrame,
  decodeViewFrameHeader,
} from "./view-frame";

interface HeaderFields {
  viewId: number;
  sequence: number;
  kind: number;
  channelCount: number;
  sampleBits: number;
  flags: number;
  itemCount: number;
  firstSample: bigint;
  samplesPerItem: bigint;
}

const DEFAULTS: HeaderFields = {
  viewId: 4,
  sequence: 1,
  kind: 2,
  channelCount: 8,
  sampleBits: 8,
  flags: 0b01,
  itemCount: 1600,
  firstSample: 3_100_000n,
  samplesPerItem: 1024n,
};

/** Builds a known-good 32-byte header (plus a 4-byte payload marker) by hand. */
function buildFrame(overrides: Partial<HeaderFields> = {}): ArrayBuffer {
  const fields = { ...DEFAULTS, ...overrides };
  const buf = new ArrayBuffer(VIEW_FRAME_HEADER_LEN + 4);
  const view = new DataView(buf);
  view.setUint8(0, 0xa6);
  view.setUint8(1, 1);
  view.setUint16(2, fields.viewId, true);
  view.setUint32(4, fields.sequence, true);
  view.setUint8(8, fields.kind);
  view.setUint8(9, fields.channelCount);
  view.setUint8(10, fields.sampleBits);
  view.setUint8(11, fields.flags);
  view.setUint32(12, fields.itemCount, true);
  view.setBigUint64(16, fields.firstSample, true);
  view.setBigUint64(24, fields.samplesPerItem, true);
  view.setUint32(32, 0xdeadbeef, true); // payload marker
  return buf;
}

describe("decodeViewFrameHeader", () => {
  it("decodes a known-good byte buffer", () => {
    const header = decodeViewFrameHeader(buildFrame());
    expect(header).toEqual({
      viewId: 4,
      sequence: 1,
      kind: "digital-summary",
      channelCount: 8,
      sampleBits: 8,
      flags: { follow: true, truncated: false },
      itemCount: 1600,
      firstSample: 3_100_000n,
      samplesPerItem: 1024n,
    });
  });

  it.each([
    [1, "digital-runs"],
    [2, "digital-summary"],
    [3, "analog-raw"],
    [4, "analog-summary"],
  ] as const)("maps kind byte %d to %s", (byte, expected) => {
    const header = decodeViewFrameHeader(buildFrame({ kind: byte }));
    expect(header.kind).toBe(expected);
  });

  it.each([
    [0b00, false, false],
    [0b01, true, false],
    [0b10, false, true],
    [0b11, true, true],
  ] as const)("decodes flags byte %d as follow=%s truncated=%s", (byte, follow, truncated) => {
    const header = decodeViewFrameHeader(buildFrame({ flags: byte }));
    expect(header.flags).toEqual({ follow, truncated });
  });

  it("rejects a bad magic byte", () => {
    const buf = buildFrame();
    new DataView(buf).setUint8(0, 0x00);
    expect(() => decodeViewFrameHeader(buf)).toThrow(ViewFrameDecodeError);
    expect(() => decodeViewFrameHeader(buf)).toThrow(/magic/);
  });

  it("rejects an unsupported version", () => {
    const buf = buildFrame();
    new DataView(buf).setUint8(1, 9);
    expect(() => decodeViewFrameHeader(buf)).toThrow(/version/);
  });

  it("rejects an unknown kind byte", () => {
    const buf = buildFrame({ kind: 0 });
    expect(() => decodeViewFrameHeader(buf)).toThrow(/kind/);
    const buf2 = buildFrame({ kind: 5 });
    expect(() => decodeViewFrameHeader(buf2)).toThrow(/kind/);
  });

  it("rejects a too-short buffer", () => {
    const buf = buildFrame().slice(0, 10);
    expect(() => decodeViewFrameHeader(buf)).toThrow(/too short/);
  });

  it("handles a first_sample above 2^53 without losing precision", () => {
    const huge = 9_007_199_254_740_993n; // 2^53 + 1
    const header = decodeViewFrameHeader(buildFrame({ firstSample: huge }));
    expect(header.firstSample).toBe(huge);
  });
});

describe("decodeViewFrame", () => {
  it("exposes the payload as a zero-copy view over the same buffer", () => {
    const buf = buildFrame();
    const { header, payload } = decodeViewFrame(buf);
    expect(header.sequence).toBe(1);
    expect(payload.byteLength).toBe(4);
    expect(payload.getUint32(0, true)).toBe(0xdeadbeef);
    expect(payload.buffer).toBe(buf);
  });
});
