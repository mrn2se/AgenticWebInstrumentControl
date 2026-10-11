/**
 * Binary sample frame header codec — the TypeScript mirror of
 * `awic-proto::frame` (SPECIFICATION.md §3.2). Kept hand-written rather than
 * generated: it is a fixed byte layout, not a serde type, so ts-rs has
 * nothing to export for it.
 *
 * ```text
 * off  size  field
 *  0    1    magic 0xA5
 *  1    1    version (1)
 *  2    2    stream_id        u16
 *  4    4    sequence         u32    per stream; detects drops
 *  8    1    encoding         u8     0=u8 interleaved, 1=i16, 2=packed digital,
 *                                    3=minmax u8, 4=minmax i16, 5=bitwise digital
 *  9    1    channel_count    u8
 * 10    2    reserved
 * 12    4    sample_count     u32    per channel
 * 16    8    t0_ns            u64    capture-relative
 * 24   ..    payload
 * ```
 */

export const FRAME_MAGIC = 0xa5;
export const FRAME_VERSION = 1;
export const FRAME_HEADER_LEN = 24;

// 3–5 are decimated previews and are self-describing on purpose: the width
// (u8 vs i16) and the semantics (numeric min/max vs bitwise AND/OR for
// packed digital) cannot be recovered from anything but the header.
export type FrameEncoding =
  | "u8-interleaved"
  | "i16"
  | "packed-digital"
  | "minmax-u8"
  | "minmax-i16"
  | "bitwise-digital";

const ENCODING_BY_BYTE: ReadonlyArray<FrameEncoding | undefined> = [
  "u8-interleaved",
  "i16",
  "packed-digital",
  "minmax-u8",
  "minmax-i16",
  "bitwise-digital",
];

export interface FrameHeader {
  readonly streamId: number;
  /** Monotonically increasing per stream; a gap means dropped frames. */
  readonly sequence: number;
  readonly encoding: FrameEncoding;
  readonly channelCount: number;
  /** Sample count *per channel* in the payload that follows. */
  readonly sampleCount: number;
  /** Capture-relative timestamp of the first sample, in nanoseconds. */
  readonly t0Ns: bigint;
}

export interface DecodedFrame {
  readonly header: FrameHeader;
  /** Zero-copy view over the payload bytes following the header. */
  readonly payload: DataView;
}

export class FrameDecodeError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "FrameDecodeError";
  }
}

/** Decodes just the 24-byte header at `byteOffset` in `buf`. */
export function decodeFrameHeader(buf: ArrayBufferLike, byteOffset = 0): FrameHeader {
  const available = buf.byteLength - byteOffset;
  if (available < FRAME_HEADER_LEN) {
    throw new FrameDecodeError(
      `frame buffer too short: need at least ${FRAME_HEADER_LEN} bytes, got ${available}`,
    );
  }
  const view = new DataView(buf, byteOffset);

  const magic = view.getUint8(0);
  if (magic !== FRAME_MAGIC) {
    throw new FrameDecodeError(
      `bad magic byte: expected 0x${FRAME_MAGIC.toString(16)}, got 0x${magic.toString(16)}`,
    );
  }
  const version = view.getUint8(1);
  if (version !== FRAME_VERSION) {
    throw new FrameDecodeError(`unsupported frame version: ${version}`);
  }

  const streamId = view.getUint16(2, true);
  const sequence = view.getUint32(4, true);
  const encodingByte = view.getUint8(8);
  const encoding = ENCODING_BY_BYTE[encodingByte];
  if (encoding === undefined) {
    throw new FrameDecodeError(`unknown encoding byte: ${encodingByte}`);
  }
  const channelCount = view.getUint8(9);
  // bytes 10..12 reserved, ignored.
  const sampleCount = view.getUint32(12, true);
  const t0Ns = view.getBigUint64(16, true);

  return { streamId, sequence, encoding, channelCount, sampleCount, t0Ns };
}

/**
 * Decodes a full frame: header plus a zero-copy `DataView` over the
 * remaining payload bytes of the same underlying `ArrayBuffer` (no slice/
 * copy — the view just starts at a nonzero byte offset).
 */
export function decodeFrame(buf: ArrayBufferLike, byteOffset = 0): DecodedFrame {
  const header = decodeFrameHeader(buf, byteOffset);
  const payload = new DataView(buf, byteOffset + FRAME_HEADER_LEN);
  return { header, payload };
}
