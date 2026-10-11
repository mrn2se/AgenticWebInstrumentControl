import { FRAME_HEADER_LEN, decodeFrame, type DecodedFrame } from "../../ws/frame";

/**
 * Hand-built sample frames for tests — the Rust server can't yet stream
 * from real hardware (per the task brief), so decoder correctness is
 * verified against wire bytes assembled by hand here, the same way
 * `ws/frame.test.ts` hand-builds headers. This goes through the real
 * `decodeFrame` (not a shortcut past it) so a fixture is exactly what a
 * `WsClient` would hand `modules/graph` from a live socket.
 */
type FrameEncodingName = "u8-interleaved" | "i16" | "packed-digital" | "minmax-u8" | "minmax-i16" | "bitwise-digital";

const ENCODING_BYTE = {
  "u8-interleaved": 0,
  i16: 1,
  "packed-digital": 2,
  "minmax-u8": 3,
  "minmax-i16": 4,
  "bitwise-digital": 5,
} satisfies Record<FrameEncodingName, number>;

export interface BuildFrameOptions {
  readonly encoding: FrameEncodingName;
  readonly channelCount: number;
  /** Sample count *per channel* — or bucket count, for the decimated encodings (SPEC §3.2). */
  readonly sampleCount: number;
  readonly payload: readonly number[];
  readonly streamId?: number;
  readonly sequence?: number;
  readonly t0Ns?: bigint;
}

/** Assembles a 24-byte header (SPEC §3.2) plus the given payload bytes, then decodes it. */
export function buildSampleFrame(options: BuildFrameOptions): DecodedFrame {
  const buf = new ArrayBuffer(FRAME_HEADER_LEN + options.payload.length);
  const view = new DataView(buf);
  view.setUint8(0, 0xa5); // magic
  view.setUint8(1, 1); // version
  view.setUint16(2, options.streamId ?? 1, true);
  view.setUint32(4, options.sequence ?? 0, true);
  view.setUint8(8, ENCODING_BYTE[options.encoding]);
  view.setUint8(9, options.channelCount);
  view.setUint16(10, 0, true); // reserved
  view.setUint32(12, options.sampleCount, true);
  view.setBigUint64(16, options.t0Ns ?? 0n, true);
  options.payload.forEach((byte, i) => view.setUint8(FRAME_HEADER_LEN + i, byte));
  return decodeFrame(buf);
}
