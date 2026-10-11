// Cross-language golden vectors for the binary view frame header (SPEC
// §3.4).
//
// view-frame.ts hand-mirrors crates/awic-proto/src/view_frame.rs: raw bytes
// are not something ts-rs can export, so codec drift would not be a compile
// error. Both sides test against this one spec-derived fixture instead —
// shared by crates/awic-proto/tests/view_frame_vectors.rs.
import { describe, expect, it } from "vitest";
import fixture from "./view-frame-vectors.json";
import { decodeViewFrameHeader, type ViewFrameKind } from "./view-frame";

const KINDS: readonly ViewFrameKind[] = [
  "digital-runs",
  "digital-summary",
  "analog-raw",
  "analog-summary",
];

function unhex(hex: string): ArrayBuffer {
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  return out.buffer;
}

describe("view frame header golden vectors", () => {
  for (const v of fixture.vectors) {
    it(`decodes: ${v.name}`, () => {
      const h = decodeViewFrameHeader(unhex(v.hex));
      expect(h.viewId).toBe(v.view_id);
      expect(h.sequence).toBe(v.sequence);
      expect(h.kind).toBe(KINDS[v.kind - 1]);
      expect(h.channelCount).toBe(v.channel_count);
      expect(h.sampleBits).toBe(v.sample_bits);
      expect(h.flags.follow).toBe((v.flags & 0b01) !== 0);
      expect(h.flags.truncated).toBe((v.flags & 0b10) !== 0);
      expect(h.itemCount).toBe(v.item_count);
      // BigInt from the decimal string: a JS number cannot hold u64 exactly.
      expect(h.firstSample).toBe(BigInt(v.first_sample));
      expect(h.samplesPerItem).toBe(BigInt(v.samples_per_item));
    });
  }
});
