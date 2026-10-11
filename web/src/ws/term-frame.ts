/**
 * Binary terminal-output frame codec — the TypeScript mirror of
 * `awic-proto::term_frame` (SPECIFICATION.md §3.5, §11.2). Hand-written for
 * the same reason `frame.ts` is: a fixed byte layout, not a serde type, so
 * ts-rs has nothing to export.
 *
 * ```text
 * off  size  field
 *  0    1    magic 0xA7
 *  1    1    version (1)
 *  2    2    session          u16
 *  4    4    sequence         u32    per session; detects drops
 *  8   ..    payload bytes
 * ```
 *
 * No `byteLen` field: a terminal frame is always exactly one WebSocket
 * binary message, and the transport already carries its length — unlike a
 * sample frame (§3.2), which a decoder must be able to parse standalone
 * from a buffer that isn't necessarily exactly one frame long.
 *
 * Distinguished from sample frames (`0xA5`, `frame.ts`) by its own magic
 * byte, so `WsClient` can dispatch on the first byte of every binary
 * message without the two ever being confused.
 */

export const TERM_FRAME_MAGIC = 0xa7;
export const TERM_FRAME_VERSION = 1;
export const TERM_FRAME_HEADER_LEN = 8;

export interface TermFrameHeader {
  readonly sessionId: number;
  /** Monotonically increasing per session; a gap means dropped output. */
  readonly sequence: number;
}

export interface DecodedTermFrame {
  readonly header: TermFrameHeader;
  /** The payload bytes, copied out (small text chunks — unlike sample
   * frames there is no reason to keep a zero-copy view alive). */
  readonly payload: Uint8Array;
}

export class TermFrameDecodeError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "TermFrameDecodeError";
  }
}

/** Returns true if `buf`'s first byte is the terminal-frame magic — the
 * dispatch `WsClient` uses to tell a `0xA7` terminal frame from a `0xA5`
 * sample frame before decoding either. */
export function isTermFrame(buf: ArrayBufferLike, byteOffset = 0): boolean {
  if (buf.byteLength - byteOffset < 1) return false;
  return new DataView(buf, byteOffset).getUint8(0) === TERM_FRAME_MAGIC;
}

export function decodeTermFrameHeader(buf: ArrayBufferLike, byteOffset = 0): TermFrameHeader {
  const available = buf.byteLength - byteOffset;
  if (available < TERM_FRAME_HEADER_LEN) {
    throw new TermFrameDecodeError(
      `frame buffer too short: need at least ${TERM_FRAME_HEADER_LEN} bytes, got ${available}`,
    );
  }
  const view = new DataView(buf, byteOffset);

  const magic = view.getUint8(0);
  if (magic !== TERM_FRAME_MAGIC) {
    throw new TermFrameDecodeError(
      `bad magic byte: expected 0x${TERM_FRAME_MAGIC.toString(16)}, got 0x${magic.toString(16)}`,
    );
  }
  const version = view.getUint8(1);
  if (version !== TERM_FRAME_VERSION) {
    throw new TermFrameDecodeError(`unsupported frame version: ${version}`);
  }

  const sessionId = view.getUint16(2, true);
  const sequence = view.getUint32(4, true);
  return { sessionId, sequence };
}

/** Decodes a full frame: header plus every remaining byte in `buf` as payload. */
export function decodeTermFrame(buf: ArrayBufferLike, byteOffset = 0): DecodedTermFrame {
  const header = decodeTermFrameHeader(buf, byteOffset);
  const payloadStart = byteOffset + TERM_FRAME_HEADER_LEN;
  const payload = new Uint8Array(buf, payloadStart, buf.byteLength - payloadStart);
  return { header, payload };
}
