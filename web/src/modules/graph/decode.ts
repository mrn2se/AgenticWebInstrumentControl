import type { DecodedFrame } from "../../ws/frame";

/**
 * Turns a decoded sample frame's raw payload bytes into structural,
 * index-addressed data — SPECIFICATION.md §3.2's six encodings, decoded
 * exactly per the (recently revised) encoding table. This module knows
 * nothing about time or volts; `traces.ts` combines its output with the
 * acquisition's configured rate and the selected channel scale to produce
 * renderer-ready `Trace`s. Keeping the two separate is what makes "decode
 * each of the six encodings correctly" independently testable from "convert
 * to seconds and volts", which needs additional context (rate, range) the
 * frame itself does not carry.
 */
export class SampleDecodeError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "SampleDecodeError";
  }
}

/** One analog channel's raw ADC codes — u8 (0..255) or signed i16, per the source encoding. */
export interface AnalogRawChannel {
  readonly channel: number;
  readonly codes: readonly number[];
}

/** One analog channel's decimated min/max envelope (SPEC §3.2 encodings 3/4). */
export interface AnalogEnvelopeChannel {
  readonly channel: number;
  readonly min: readonly number[];
  readonly max: readonly number[];
}

export type DecodedFrameData =
  | { readonly kind: "analog-raw"; readonly channels: readonly AnalogRawChannel[] }
  | { readonly kind: "analog-envelope"; readonly channels: readonly AnalogEnvelopeChannel[] }
  | { readonly kind: "digital-raw"; readonly channelCount: number; readonly samples: readonly number[] }
  | {
      readonly kind: "digital-bitwise";
      readonly channelCount: number;
      readonly and: readonly number[];
      readonly or: readonly number[];
    };

function requireByteLength(frame: DecodedFrame, expected: number): void {
  if (frame.payload.byteLength !== expected) {
    throw new SampleDecodeError(
      `${frame.header.encoding} frame: expected ${expected} payload bytes for ` +
        `${frame.header.channelCount} channel(s) x ${frame.header.sampleCount} sample(s), ` +
        `got ${frame.payload.byteLength}`,
    );
  }
}

/**
 * Encodings 0/1 are **interleaved**: `ch0,ch1,ch0,ch1,...` — sample index
 * `i`'s value for channel `ch` sits at element `i * channelCount + ch`.
 */
function decodeU8Interleaved(frame: DecodedFrame): DecodedFrameData {
  const { channelCount, sampleCount } = frame.header;
  requireByteLength(frame, channelCount * sampleCount);
  const channels: AnalogRawChannel[] = [];
  for (let ch = 0; ch < channelCount; ch++) {
    const codes: number[] = new Array(sampleCount);
    for (let i = 0; i < sampleCount; i++) {
      codes[i] = frame.payload.getUint8(i * channelCount + ch);
    }
    channels.push({ channel: ch, codes });
  }
  return { kind: "analog-raw", channels };
}

function decodeI16Interleaved(frame: DecodedFrame): DecodedFrameData {
  const { channelCount, sampleCount } = frame.header;
  requireByteLength(frame, channelCount * sampleCount * 2);
  const channels: AnalogRawChannel[] = [];
  for (let ch = 0; ch < channelCount; ch++) {
    const codes: number[] = new Array(sampleCount);
    for (let i = 0; i < sampleCount; i++) {
      codes[i] = frame.payload.getInt16((i * channelCount + ch) * 2, true);
    }
    channels.push({ channel: ch, codes });
  }
  return { kind: "analog-raw", channels };
}

/** Encoding 2: one byte per sample, all channels packed as bits — not interleaved per-channel bytes. */
function decodePackedDigital(frame: DecodedFrame): DecodedFrameData {
  const { channelCount, sampleCount } = frame.header;
  requireByteLength(frame, sampleCount);
  const samples: number[] = new Array(sampleCount);
  for (let i = 0; i < sampleCount; i++) {
    samples[i] = frame.payload.getUint8(i);
  }
  return { kind: "digital-raw", channelCount, samples };
}

/**
 * Encodings 3/4 are **channel-major**, the opposite layout from 0/1: all of
 * channel 0's `(min, max)` pairs first, then all of channel 1's, and so on.
 * Channel `ch`'s pairs start at byte offset `ch * sampleCount * pairSize`.
 */
function decodeMinMaxU8(frame: DecodedFrame): DecodedFrameData {
  const { channelCount, sampleCount } = frame.header;
  requireByteLength(frame, channelCount * sampleCount * 2);
  const channels: AnalogEnvelopeChannel[] = [];
  for (let ch = 0; ch < channelCount; ch++) {
    const base = ch * sampleCount * 2;
    const min: number[] = new Array(sampleCount);
    const max: number[] = new Array(sampleCount);
    for (let i = 0; i < sampleCount; i++) {
      min[i] = frame.payload.getUint8(base + i * 2);
      max[i] = frame.payload.getUint8(base + i * 2 + 1);
    }
    channels.push({ channel: ch, min, max });
  }
  return { kind: "analog-envelope", channels };
}

function decodeMinMaxI16(frame: DecodedFrame): DecodedFrameData {
  const { channelCount, sampleCount } = frame.header;
  requireByteLength(frame, channelCount * sampleCount * 4);
  const channels: AnalogEnvelopeChannel[] = [];
  for (let ch = 0; ch < channelCount; ch++) {
    const base = ch * sampleCount * 4;
    const min: number[] = new Array(sampleCount);
    const max: number[] = new Array(sampleCount);
    for (let i = 0; i < sampleCount; i++) {
      min[i] = frame.payload.getInt16(base + i * 4, true);
      max[i] = frame.payload.getInt16(base + i * 4 + 2, true);
    }
    channels.push({ channel: ch, min, max });
  }
  return { kind: "analog-envelope", channels };
}

/**
 * Encoding 5: **one stream** — not channel-major, not interleaved — of
 * `buckets x (AND u8, OR u8)`. Splitting this bitmask pair into per-channel
 * bucket states is `splitBitwiseChannels`'s job, not this function's: this
 * layer only extracts the two byte arrays the header promises.
 */
function decodeBitwiseDigital(frame: DecodedFrame): DecodedFrameData {
  const { channelCount, sampleCount } = frame.header;
  requireByteLength(frame, sampleCount * 2);
  const and: number[] = new Array(sampleCount);
  const or: number[] = new Array(sampleCount);
  for (let i = 0; i < sampleCount; i++) {
    and[i] = frame.payload.getUint8(i * 2);
    or[i] = frame.payload.getUint8(i * 2 + 1);
  }
  return { kind: "digital-bitwise", channelCount, and, or };
}

export function decodeSampleFrame(frame: DecodedFrame): DecodedFrameData {
  switch (frame.header.encoding) {
    case "u8-interleaved":
      return decodeU8Interleaved(frame);
    case "i16":
      return decodeI16Interleaved(frame);
    case "packed-digital":
      return decodePackedDigital(frame);
    case "minmax-u8":
      return decodeMinMaxU8(frame);
    case "minmax-i16":
      return decodeMinMaxI16(frame);
    case "bitwise-digital":
      return decodeBitwiseDigital(frame);
    default: {
      const exhaustive: never = frame.header.encoding;
      throw new SampleDecodeError(`decodeSampleFrame: unhandled encoding ${JSON.stringify(exhaustive)}`);
    }
  }
}

/** Bit `n` of `byte` — channel D`n` (SPEC §3.2: "bit n = channel Dn"). */
export function digitalBit(byte: number, n: number): 0 | 1 {
  return ((byte >> n) & 1) as 0 | 1;
}

export interface DigitalChannelLevels {
  readonly channel: number;
  readonly levels: readonly (0 | 1)[];
}

/** Splits encoding-2 packed-digital samples into one 0/1 level array per channel bit. */
export function splitDigitalChannels(
  data: Extract<DecodedFrameData, { kind: "digital-raw" }>,
): DigitalChannelLevels[] {
  const channels: DigitalChannelLevels[] = [];
  for (let ch = 0; ch < data.channelCount; ch++) {
    channels.push({ channel: ch, levels: data.samples.map((byte) => digitalBit(byte, ch)) });
  }
  return channels;
}

export type BucketState = "low" | "high" | "toggled";

export interface DigitalChannelBuckets {
  readonly channel: number;
  readonly states: readonly BucketState[];
}

/**
 * **The digital decimation rule (SPEC §3.2), the part that is easy to get
 * wrong:** encoding 5's AND/OR bytes are bitmasks, not numbers. Per channel
 * bit within one bucket: `(0,0)` is low throughout, `(1,1)` is high
 * throughout, and any disagreement between the AND bit and the OR bit means
 * the channel changed state at least once inside the bucket — `"toggled"`,
 * which must be rendered as a transition marker spanning the bucket's full
 * height, never averaged or dropped into a level. `(AND=1, OR=0)` cannot
 * happen for genuine AND/OR data (AND implies OR) but is treated as
 * disagreement too, defensively, rather than silently reported as a level.
 */
export function splitBitwiseChannels(
  data: Extract<DecodedFrameData, { kind: "digital-bitwise" }>,
): DigitalChannelBuckets[] {
  const channels: DigitalChannelBuckets[] = [];
  for (let ch = 0; ch < data.channelCount; ch++) {
    const states: BucketState[] = data.and.map((andByte, i) => {
      const orByte = data.or[i];
      if (orByte === undefined) {
        throw new SampleDecodeError(`bitwise-digital frame: AND/OR bucket count mismatch at index ${i}`);
      }
      const andBit = digitalBit(andByte, ch);
      const orBit = digitalBit(orByte, ch);
      if (andBit !== orBit) return "toggled";
      return andBit === 1 ? "high" : "low";
    });
    channels.push({ channel: ch, states });
  }
  return channels;
}
