import type { DecodedViewFrame } from "../../ws/view-frame";

/**
 * Structural decode of a view frame's payload (SPECIFICATION.md §3.4) — the
 * view-frame counterpart of `decode.ts` for `0xA5` sample frames. Kept
 * separate from `view-traces.ts` for the same reason `decode.ts` is kept
 * separate from `traces.ts`: "decode the wire bytes correctly" and "place
 * them in time/volts" are independently testable concerns, and only the
 * latter needs the recording's rate and calibration.
 */
export class ViewDecodeError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ViewDecodeError";
  }
}

/** One run item of a `digital-runs` frame: `value` is the packed byte (bit *n* = D*n*), `length` its span in samples. */
export interface DigitalRun {
  readonly value: number;
  readonly length: number;
}

/** Reads one unsigned LEB128 value starting at `offset`; returns the value and the offset just past it. */
function readLeb128(view: DataView, offset: number): { readonly value: number; readonly next: number } {
  let result = 0;
  let multiplier = 1;
  let pos = offset;
  for (;;) {
    if (pos >= view.byteLength) {
      throw new ViewDecodeError(`digital-runs payload: truncated LEB128 length at byte ${pos}`);
    }
    const byte = view.getUint8(pos);
    pos++;
    // Accumulated by multiplication, not `<<`, so a length beyond 2^31 (a
    // channel idle for a very long recording) doesn't wrap through JS's
    // 32-bit bitwise operators.
    result += (byte & 0x7f) * multiplier;
    if ((byte & 0x80) === 0) break;
    multiplier *= 128;
  }
  return { value: result, next: pos };
}

/** Kind 1: `item_count` x `(value u8, length LEB128)` — exact edges (SPEC §3.4). */
export function decodeDigitalRuns(payload: DataView, itemCount: number): DigitalRun[] {
  const runs: DigitalRun[] = new Array(itemCount);
  let offset = 0;
  for (let i = 0; i < itemCount; i++) {
    if (offset >= payload.byteLength) {
      throw new ViewDecodeError(`digital-runs payload: expected ${itemCount} runs, ran out of bytes at item ${i}`);
    }
    const value = payload.getUint8(offset);
    offset++;
    const { value: length, next } = readLeb128(payload, offset);
    offset = next;
    runs[i] = { value, length };
  }
  return runs;
}

export interface DigitalSummaryItem {
  readonly and: number;
  readonly or: number;
}

/** Kind 2: `item_count` x `(AND u8, OR u8)` — same bitwise-summary semantics as SPEC §3.2 encoding 5. */
export function decodeDigitalSummary(payload: DataView, itemCount: number): DigitalSummaryItem[] {
  const expected = itemCount * 2;
  if (payload.byteLength < expected) {
    throw new ViewDecodeError(`digital-summary payload: expected ${expected} bytes for ${itemCount} items, got ${payload.byteLength}`);
  }
  const items: DigitalSummaryItem[] = new Array(itemCount);
  for (let i = 0; i < itemCount; i++) {
    items[i] = { and: payload.getUint8(i * 2), or: payload.getUint8(i * 2 + 1) };
  }
  return items;
}

/** One analog channel's raw codes (kind 3) — `sampleBits` wide, interleaved by channel like SPEC §3.2 encodings 0/1. */
export interface AnalogRawChannel {
  readonly channel: number;
  readonly codes: readonly number[];
}

export function decodeAnalogRaw(
  payload: DataView,
  channelCount: number,
  itemCount: number,
  sampleBits: number,
): AnalogRawChannel[] {
  const bytesPerSample = sampleBits === 16 ? 2 : 1;
  const expected = channelCount * itemCount * bytesPerSample;
  if (payload.byteLength < expected) {
    throw new ViewDecodeError(`analog-raw payload: expected ${expected} bytes, got ${payload.byteLength}`);
  }
  const channels: AnalogRawChannel[] = [];
  for (let ch = 0; ch < channelCount; ch++) {
    const codes: number[] = new Array(itemCount);
    for (let i = 0; i < itemCount; i++) {
      const offset = (i * channelCount + ch) * bytesPerSample;
      codes[i] = sampleBits === 16 ? payload.getInt16(offset, true) : payload.getUint8(offset);
    }
    channels.push({ channel: ch, codes });
  }
  return channels;
}

/** One analog channel's decimated min/max envelope (kind 4), channel-major like SPEC §3.2 encodings 3/4. */
export interface AnalogSummaryChannel {
  readonly channel: number;
  readonly min: readonly number[];
  readonly max: readonly number[];
}

export function decodeAnalogSummary(
  payload: DataView,
  channelCount: number,
  itemCount: number,
  sampleBits: number,
): AnalogSummaryChannel[] {
  const bytesPerSample = sampleBits === 16 ? 2 : 1;
  const pairSize = bytesPerSample * 2;
  const expected = channelCount * itemCount * pairSize;
  if (payload.byteLength < expected) {
    throw new ViewDecodeError(`analog-summary payload: expected ${expected} bytes, got ${payload.byteLength}`);
  }
  const channels: AnalogSummaryChannel[] = [];
  for (let ch = 0; ch < channelCount; ch++) {
    const base = ch * itemCount * pairSize;
    const min: number[] = new Array(itemCount);
    const max: number[] = new Array(itemCount);
    for (let i = 0; i < itemCount; i++) {
      const offset = base + i * pairSize;
      if (sampleBits === 16) {
        min[i] = payload.getInt16(offset, true);
        max[i] = payload.getInt16(offset + 2, true);
      } else {
        min[i] = payload.getUint8(offset);
        max[i] = payload.getUint8(offset + 1);
      }
    }
    channels.push({ channel: ch, min, max });
  }
  return channels;
}

export type DecodedViewData =
  | { readonly kind: "digital-runs"; readonly runs: readonly DigitalRun[] }
  | { readonly kind: "digital-summary"; readonly items: readonly DigitalSummaryItem[] }
  | { readonly kind: "analog-raw"; readonly channels: readonly AnalogRawChannel[] }
  | { readonly kind: "analog-summary"; readonly channels: readonly AnalogSummaryChannel[] };

/** Dispatches on the frame's `header.kind`, mirroring `decodeSampleFrame`'s shape for `0xA5` frames. */
export function decodeViewFramePayload(frame: DecodedViewFrame): DecodedViewData {
  const { header, payload } = frame;
  switch (header.kind) {
    case "digital-runs":
      return { kind: "digital-runs", runs: decodeDigitalRuns(payload, header.itemCount) };
    case "digital-summary":
      return { kind: "digital-summary", items: decodeDigitalSummary(payload, header.itemCount) };
    case "analog-raw":
      return {
        kind: "analog-raw",
        channels: decodeAnalogRaw(payload, header.channelCount, header.itemCount, header.sampleBits),
      };
    case "analog-summary":
      return {
        kind: "analog-summary",
        channels: decodeAnalogSummary(payload, header.channelCount, header.itemCount, header.sampleBits),
      };
    default: {
      const exhaustive: never = header.kind;
      throw new ViewDecodeError(`decodeViewFramePayload: unhandled kind ${JSON.stringify(exhaustive)}`);
    }
  }
}
