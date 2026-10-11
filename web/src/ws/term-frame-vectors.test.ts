// Cross-language golden vectors for the terminal frame header (SPEC §3.5).
//
// term-frame.ts hand-mirrors crates/awic-proto/src/term_frame.rs: raw bytes
// are not something ts-rs can export, so codec drift would not be a compile
// error. Both sides test against this one spec-derived fixture instead.
import { describe, expect, it } from "vitest";
import fixture from "./term-frame-vectors.json";
import { decodeTermFrameHeader, isTermFrame } from "./term-frame";

function unhex(hex: string): ArrayBuffer {
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  return out.buffer;
}

describe("terminal frame header golden vectors", () => {
  for (const v of fixture.vectors) {
    it(`decodes: ${v.name}`, () => {
      const buf = unhex(v.hex);
      expect(isTermFrame(buf)).toBe(true);
      const h = decodeTermFrameHeader(buf);
      expect(h.sessionId).toBe(v.session_id);
      expect(h.sequence).toBe(v.sequence);
    });
  }
});
