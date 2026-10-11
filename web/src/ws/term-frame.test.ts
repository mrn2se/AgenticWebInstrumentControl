import { describe, expect, it } from "vitest";
import {
  TERM_FRAME_HEADER_LEN,
  TermFrameDecodeError,
  decodeTermFrame,
  decodeTermFrameHeader,
  isTermFrame,
} from "./term-frame";

function buildFrame(sessionId: number, sequence: number, payload: Uint8Array): ArrayBuffer {
  const buf = new ArrayBuffer(TERM_FRAME_HEADER_LEN + payload.length);
  const view = new DataView(buf);
  view.setUint8(0, 0xa7);
  view.setUint8(1, 1);
  view.setUint16(2, sessionId, true);
  view.setUint32(4, sequence, true);
  new Uint8Array(buf, TERM_FRAME_HEADER_LEN).set(payload);
  return buf;
}

describe("isTermFrame", () => {
  it("is true for a 0xA7 buffer and false for a 0xA5 (sample frame) buffer", () => {
    expect(isTermFrame(buildFrame(1, 0, new Uint8Array()))).toBe(true);
    const sample = new ArrayBuffer(1);
    new DataView(sample).setUint8(0, 0xa5);
    expect(isTermFrame(sample)).toBe(false);
  });

  it("is false for an empty buffer rather than throwing", () => {
    expect(isTermFrame(new ArrayBuffer(0))).toBe(false);
  });
});

describe("decodeTermFrame", () => {
  it("decodes a header and returns every remaining byte as payload", () => {
    const text = new TextEncoder().encode("hello uart\n");
    const buf = buildFrame(3, 9, text);
    const { header, payload } = decodeTermFrame(buf);
    expect(header).toEqual({ sessionId: 3, sequence: 9 });
    expect(new TextDecoder().decode(payload)).toBe("hello uart\n");
  });

  it("rejects a bad magic byte", () => {
    const buf = buildFrame(0, 0, new Uint8Array());
    new DataView(buf).setUint8(0, 0x00);
    expect(() => decodeTermFrameHeader(buf)).toThrow(TermFrameDecodeError);
    expect(() => decodeTermFrameHeader(buf)).toThrow(/magic/);
  });

  it("rejects an unsupported version", () => {
    const buf = buildFrame(0, 0, new Uint8Array());
    new DataView(buf).setUint8(1, 9);
    expect(() => decodeTermFrameHeader(buf)).toThrow(/version/);
  });

  it("rejects a too-short buffer", () => {
    expect(() => decodeTermFrameHeader(new ArrayBuffer(4))).toThrow(/too short/);
  });
});
