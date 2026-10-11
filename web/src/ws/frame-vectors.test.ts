// Cross-language golden vectors for the binary frame header (SPEC §3.2).
//
// frame.ts hand-mirrors crates/awic-proto/src/frame.rs: raw bytes are not
// something ts-rs can export, so codec drift would not be a compile error.
// Both sides test against this one spec-derived fixture instead.
import { describe, expect, it } from "vitest";
import fixture from "./frame-vectors.json";
import { decodeFrameHeader, type FrameEncoding } from "./frame";

const ENCODINGS: readonly FrameEncoding[] = [
  "u8-interleaved",
  "i16",
  "packed-digital",
  "minmax-u8",
  "minmax-i16",
  "bitwise-digital",
];

function unhex(hex: string): ArrayBuffer {
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  return out.buffer;
}

describe("frame header golden vectors", () => {
  for (const v of fixture.vectors) {
    it(`decodes: ${v.name}`, () => {
      const h = decodeFrameHeader(unhex(v.hex));
      expect(h.streamId).toBe(v.stream_id);
      expect(h.sequence).toBe(v.sequence);
      expect(h.encoding).toBe(ENCODINGS[v.encoding]);
      expect(h.channelCount).toBe(v.channel_count);
      expect(h.sampleCount).toBe(v.sample_count);
      // BigInt from the decimal string: a JS number cannot hold u64 exactly.
      expect(h.t0Ns).toBe(BigInt(v.t0_ns));
    });
  }
});
