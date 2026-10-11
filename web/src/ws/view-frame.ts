/**
 * Binary view frame header codec — the TypeScript mirror of
 * `awic-proto::view_frame` (SPECIFICATION.md §3.4). Hand-written rather than
 * generated, exactly like `frame.ts`: this is a fixed byte layout, not a
 * serde type, so ts-rs has nothing to export for it.
 *
 * ```text
 * off  size  field
 *  0    1    magic 0xA6
 *  1    1    version (1)
 *  2    2    view_id          u16
 *  4    4    sequence         u32    per view
 *  8    1    kind             u8     1=digital runs, 2=digital summary, 3=analog raw, 4=analog summary
 *  9    1    channel_count    u8
 * 10    1    sample_bits      u8     8 or 16 (analog); 8 (digital)
 * 11    1    flags            u8     bit 0: follow view; bit 1: range truncated by eviction
 * 12    4    item_count       u32
 * 16    8    first_sample     u64    absolute sample index of the first item
 * 24    8    samples_per_item u64    1 for runs and raw samples
 * 32   ..    payload
 * ```
 *
 * Unlike the 24-byte §3.2 sample frame, a view frame is placeable on its
 * own: `firstSample`, `samplesPerItem` and the recording's `rate_hz` (from
 * `rec.list`) give every item an exact time span.
 */

export const VIEW_FRAME_MAGIC = 0xa6;
export const VIEW_FRAME_VERSION = 1;
export const VIEW_FRAME_HEADER_LEN = 32;

export type ViewFrameKind = "digital-runs" | "digital-summary" | "analog-raw" | "analog-summary";

const KIND_BY_BYTE: ReadonlyArray<ViewFrameKind | undefined> = [
  undefined, // 0: not a valid kind
  "digital-runs",
  "digital-summary",
  "analog-raw",
  "analog-summary",
];

/** Independent bits, not a "which encoding" tag — a frame can be both a
 * follow view and truncated. */
export interface ViewFrameFlags {
  readonly follow: boolean;
  readonly truncated: boolean;
}

export interface ViewFrameHeader {
  readonly viewId: number;
  /** Monotonically increasing per view; a gap means a conflated (dropped,
   * never queued) follow-view tick. */
  readonly sequence: number;
  readonly kind: ViewFrameKind;
  readonly channelCount: number;
  /** 8 or 16 for analog; 8 for digital. */
  readonly sampleBits: number;
  readonly flags: ViewFrameFlags;
  readonly itemCount: number;
  /** Absolute sample index of the first item. */
  readonly firstSample: bigint;
  /** 1 for `digital-runs`/`analog-raw`; a power of two otherwise, aligned so
   * item boundaries are stable while a window scrolls. */
  readonly samplesPerItem: bigint;
}

export interface DecodedViewFrame {
  readonly header: ViewFrameHeader;
  /** Zero-copy view over the payload bytes following the header. */
  readonly payload: DataView;
}

export class ViewFrameDecodeError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ViewFrameDecodeError";
  }
}

function decodeFlags(byte: number): ViewFrameFlags {
  return {
    follow: (byte & 0b01) !== 0,
    truncated: (byte & 0b10) !== 0,
  };
}

/** Decodes just the 32-byte header at `byteOffset` in `buf`. */
export function decodeViewFrameHeader(buf: ArrayBufferLike, byteOffset = 0): ViewFrameHeader {
  const available = buf.byteLength - byteOffset;
  if (available < VIEW_FRAME_HEADER_LEN) {
    throw new ViewFrameDecodeError(
      `view frame buffer too short: need at least ${VIEW_FRAME_HEADER_LEN} bytes, got ${available}`,
    );
  }
  const view = new DataView(buf, byteOffset);

  const magic = view.getUint8(0);
  if (magic !== VIEW_FRAME_MAGIC) {
    throw new ViewFrameDecodeError(
      `bad magic byte: expected 0x${VIEW_FRAME_MAGIC.toString(16)}, got 0x${magic.toString(16)}`,
    );
  }
  const version = view.getUint8(1);
  if (version !== VIEW_FRAME_VERSION) {
    throw new ViewFrameDecodeError(`unsupported view frame version: ${version}`);
  }

  const viewId = view.getUint16(2, true);
  const sequence = view.getUint32(4, true);
  const kindByte = view.getUint8(8);
  const kind = KIND_BY_BYTE[kindByte];
  if (kind === undefined) {
    throw new ViewFrameDecodeError(`unknown view frame kind: ${kindByte}`);
  }
  const channelCount = view.getUint8(9);
  const sampleBits = view.getUint8(10);
  const flags = decodeFlags(view.getUint8(11));
  const itemCount = view.getUint32(12, true);
  const firstSample = view.getBigUint64(16, true);
  const samplesPerItem = view.getBigUint64(24, true);

  return {
    viewId,
    sequence,
    kind,
    channelCount,
    sampleBits,
    flags,
    itemCount,
    firstSample,
    samplesPerItem,
  };
}

/**
 * Decodes a full view frame: header plus a zero-copy `DataView` over the
 * remaining payload bytes of the same underlying `ArrayBuffer`.
 */
export function decodeViewFrame(buf: ArrayBufferLike, byteOffset = 0): DecodedViewFrame {
  const header = decodeViewFrameHeader(buf, byteOffset);
  const payload = new DataView(buf, byteOffset + VIEW_FRAME_HEADER_LEN);
  return { header, payload };
}
