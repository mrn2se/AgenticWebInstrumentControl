import { describe, expect, it } from "vitest";
import { FRAME_HEADER_LEN, FrameDecodeError, decodeFrame, decodeFrameHeader } from "./frame";

interface HeaderFields {
  streamId: number;
  sequence: number;
  encoding: number;
  channelCount: number;
  sampleCount: number;
  t0Ns: bigint;
}

const DEFAULTS: HeaderFields = {
  streamId: 7,
  sequence: 42,
  encoding: 1,
  channelCount: 2,
  sampleCount: 1024,
  t0Ns: 123456789n,
};

/** Builds a known-good 24-byte header (plus a 4-byte payload marker) by hand. */
function buildFrame(overrides: Partial<HeaderFields> = {}): ArrayBuffer {
  const fields = { ...DEFAULTS, ...overrides };
  const buf = new ArrayBuffer(FRAME_HEADER_LEN + 4);
  const view = new DataView(buf);
  view.setUint8(0, 0xa5);
  view.setUint8(1, 1);
  view.setUint16(2, fields.streamId, true);
  view.setUint32(4, fields.sequence, true);
  view.setUint8(8, fields.encoding);
  view.setUint8(9, fields.channelCount);
  view.setUint16(10, 0, true); // reserved
  view.setUint32(12, fields.sampleCount, true);
  view.setBigUint64(16, fields.t0Ns, true);
  view.setUint32(24, 0xdeadbeef, true); // payload marker
  return buf;
}

describe("decodeFrameHeader", () => {
  it("decodes a known-good byte buffer", () => {
    const header = decodeFrameHeader(buildFrame());
    expect(header).toEqual({
      streamId: 7,
      sequence: 42,
      encoding: "i16",
      channelCount: 2,
      sampleCount: 1024,
      t0Ns: 123456789n,
    });
  });

  it.each([
    [0, "u8-interleaved"],
    [1, "i16"],
    [2, "packed-digital"],
    [3, "minmax-u8"],
    [4, "minmax-i16"],
    [5, "bitwise-digital"],
  ] as const)("maps encoding byte %d to %s", (byte, expected) => {
    const header = decodeFrameHeader(buildFrame({ encoding: byte }));
    expect(header.encoding).toBe(expected);
  });

  it("rejects a bad magic byte", () => {
    const buf = buildFrame();
    new DataView(buf).setUint8(0, 0x00);
    expect(() => decodeFrameHeader(buf)).toThrow(FrameDecodeError);
    expect(() => decodeFrameHeader(buf)).toThrow(/magic/);
  });

  it("rejects an unsupported version", () => {
    const buf = buildFrame();
    new DataView(buf).setUint8(1, 9);
    expect(() => decodeFrameHeader(buf)).toThrow(/version/);
  });

  it("rejects an unknown encoding byte", () => {
    const buf = buildFrame({ encoding: 200 });
    expect(() => decodeFrameHeader(buf)).toThrow(/encoding/);
  });

  it("rejects a too-short buffer", () => {
    const buf = buildFrame().slice(0, 10);
    expect(() => decodeFrameHeader(buf)).toThrow(/too short/);
  });
});

describe("decodeFrame", () => {
  it("exposes the payload as a zero-copy view over the same buffer", () => {
    const buf = buildFrame();
    const { header, payload } = decodeFrame(buf);
    expect(header.sequence).toBe(42);
    expect(payload.byteLength).toBe(4);
    expect(payload.getUint32(0, true)).toBe(0xdeadbeef);
    expect(payload.buffer).toBe(buf);
  });
});
