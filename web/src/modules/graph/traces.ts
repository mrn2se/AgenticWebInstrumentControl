import type { DecodedFrame, FrameEncoding } from "../../ws/frame";
import type { AnalogEnvelopeTrace, AnalogTrace, DigitalSegment, DigitalTrace, Trace } from "../../render/types";
import { type BucketState, decodeSampleFrame, splitBitwiseChannels, splitDigitalChannels } from "./decode";
import type { ChannelCalibration } from "../../proto/types";
import { codeToUnits } from "./scale";

/**
 * Everything needed to place a frame's samples on a time axis. The wire
 * frame header (SPEC §3.2) gives a sample/bucket *count*, never a duration
 * or rate — those live in the acquisition's configuration, not the frame.
 * For raw encodings (0/1/2) the per-sample interval is `1/rateHz` directly.
 * For decimated encodings (3/4/5) the frame gives a bucket *count* with no
 * bucket-duration field, so its buckets are spread over `frameSpanSeconds` -
 * the time the frame covers, which [`FrameSpanPlacer`] works out from the
 * frames around it. Without it a decimated frame is taken to tile the whole
 * configured capture, `(totalSamples/rateHz) / bucketCount` per bucket: true
 * only of a capture sent as one frame. A capture streamed as many frames
 * falls back to decimated ones under backpressure (SPEC §3.2) part-way
 * through, and each of those 8 ms frames was drawn the length of the whole
 * capture, overlapping the next and running past its end (#48).
 */
export interface TimebaseContext {
  readonly rateHz: number;
  /** Total configured samples per channel for the whole capture (`AcqConfig.samples`). */
  readonly totalSamples: number;
  /** Seconds this frame covers, when known - see [`FrameSpanPlacer`]. Only decimated frames need it. */
  readonly frameSpanSeconds?: number;
}

function bucketDurationSeconds(ctx: TimebaseContext, bucketCount: number): number {
  if (bucketCount <= 0) return 0;
  if (ctx.frameSpanSeconds !== undefined) return ctx.frameSpanSeconds / bucketCount;
  if (ctx.rateHz <= 0) return 0;
  return ctx.totalSamples / ctx.rateHz / bucketCount;
}

function levelsToStates(levels: readonly (0 | 1)[]): BucketState[] {
  return levels.map((level) => (level === 1 ? "high" : "low"));
}

/** Run-length encodes consecutive equal bucket states into segments, per SPEC §3.2's toggle rule. */
function runLengthSegments(states: readonly BucketState[], t0Seconds: number, dt: number): DigitalSegment[] {
  const segments: DigitalSegment[] = [];
  let index = 0;
  while (index < states.length) {
    const state = states[index];
    if (state === undefined) break;
    let end = index + 1;
    while (end < states.length && states[end] === state) end++;
    segments.push({ t0: t0Seconds + index * dt, t1: t0Seconds + end * dt, state });
    index = end;
  }
  return segments;
}

export interface TraceContext {
  readonly timebase: TimebaseContext;
  /**
   * The driver's raw-to-physical conversion for a channel, from the
   * `acq.configure` result (see `scale.ts`).
   */
  readonly calibrationForChannel: (channel: number) => ChannelCalibration;
  readonly labelForChannel: (kind: "analog" | "digital", channel: number) => string;
  readonly laneForChannel: (channel: number) => number;
  /**
   * The hardware channels a digital frame carries - the enabled ones. A frame's
   * `channel_count` counts them but each stays on its own bit (#45); without
   * this, D0..D(channelCount-1) is assumed.
   */
  readonly digitalChannels?: readonly number[];
}

/**
 * Turns one decoded frame into renderer-ready `Trace`s (SPECIFICATION.md
 * §5.3/§5.4's "drawable traces"). Structural decoding (`decode.ts`) and
 * time/volts placement (`ctx`) are deliberately separate — see
 * `TimebaseContext`'s doc comment for why the latter needs information the
 * frame itself does not carry.
 */
export function frameToTraces(frame: DecodedFrame, ctx: TraceContext): Trace[] {
  const data = decodeSampleFrame(frame);
  const t0Seconds = Number(frame.header.t0Ns) / 1e9;

  switch (data.kind) {
    case "analog-raw": {
      const dt = ctx.timebase.rateHz > 0 ? 1 / ctx.timebase.rateHz : 0;
      return data.channels.map((channel): AnalogTrace => {
        const cal = ctx.calibrationForChannel(channel.channel);
        return {
          kind: "analog",
          channel: channel.channel,
          label: ctx.labelForChannel("analog", channel.channel),
          t: channel.codes.map((_, i) => t0Seconds + i * dt),
          v: channel.codes.map((code) => codeToUnits(code, cal)),
        };
      });
    }

    case "analog-envelope": {
      const dt = bucketDurationSeconds(ctx.timebase, frame.header.sampleCount);
      return data.channels.map((channel): AnalogEnvelopeTrace => {
        const cal = ctx.calibrationForChannel(channel.channel);
        return {
          kind: "analog-envelope",
          channel: channel.channel,
          label: ctx.labelForChannel("analog", channel.channel),
          t0: channel.min.map((_, i) => t0Seconds + i * dt),
          t1: channel.min.map((_, i) => t0Seconds + (i + 1) * dt),
          vMin: channel.min.map((code) => codeToUnits(code, cal)),
          vMax: channel.max.map((code) => codeToUnits(code, cal)),
        };
      });
    }

    case "digital-raw": {
      const dt = ctx.timebase.rateHz > 0 ? 1 / ctx.timebase.rateHz : 0;
      return splitDigitalChannels(data, ctx.digitalChannels).map(
        (channel): DigitalTrace => ({
          kind: "digital",
          channel: channel.channel,
          label: ctx.labelForChannel("digital", channel.channel),
          lane: ctx.laneForChannel(channel.channel),
          segments: runLengthSegments(levelsToStates(channel.levels), t0Seconds, dt),
        }),
      );
    }

    case "digital-bitwise": {
      const dt = bucketDurationSeconds(ctx.timebase, frame.header.sampleCount);
      return splitBitwiseChannels(data, ctx.digitalChannels).map(
        (channel): DigitalTrace => ({
          kind: "digital",
          channel: channel.channel,
          label: ctx.labelForChannel("digital", channel.channel),
          lane: ctx.laneForChannel(channel.channel),
          segments: runLengthSegments(channel.states, t0Seconds, dt),
        }),
      );
    }

    default: {
      const exhaustive: never = data;
      throw new Error(`frameToTraces: unhandled decoded frame kind ${JSON.stringify(exhaustive)}`);
    }
  }
}

/** A frame and, for a decimated one, the seconds it covers (`TimebaseContext.frameSpanSeconds`). */
export interface PlacedFrame {
  readonly frame: DecodedFrame;
  readonly spanSeconds?: number;
}

function isDecimated(encoding: FrameEncoding): boolean {
  return encoding === "minmax-u8" || encoding === "minmax-i16" || encoding === "bitwise-digital";
}

function frameStartSeconds(frame: DecodedFrame): number {
  return Number(frame.header.t0Ns) / 1e9;
}

/**
 * Works out how long each decimated frame of one capture lasts (#48). The
 * header carries a decimated frame's bucket count but not the samples behind
 * it; a capture's frames are consecutive chunks, though, so a frame lasts
 * until the next one starts. A decimated frame is therefore held until the
 * next frame arrives, or until [`finish`] names the capture's end. Where the
 * sequence number shows a frame dropped in between (SPEC §3.2), the gap is
 * more than one chunk, and the last chunk length seen stands in instead.
 * Raw frames place themselves and pass straight through.
 */
export class FrameSpanPlacer {
  private pending: DecodedFrame | null = null;
  private chunkSeconds: number | null = null;
  private endSeconds: number | null = null;

  constructor(private readonly rateHz: number) {}

  /** The frames now placed, in order: `frame` itself, and the decimated one held before it. */
  push(frame: DecodedFrame): PlacedFrame[] {
    const placed: PlacedFrame[] = [];
    if (this.pending !== null) placed.push(this.place(this.pending, frame));
    this.pending = null;
    if (!isDecimated(frame.header.encoding)) {
      if (this.rateHz > 0) this.chunkSeconds = frame.header.sampleCount / this.rateHz;
      placed.push({ frame });
    } else if (this.endSeconds !== null) {
      placed.push(this.place(frame, null)); // arrived after the capture's end was announced
    } else {
      this.pending = frame;
    }
    return placed;
  }

  /** The capture ended at `endSeconds` (on the frames' time axis): places the frame still held, if any. */
  finish(endSeconds: number): PlacedFrame[] {
    this.endSeconds = endSeconds;
    const pending = this.pending;
    this.pending = null;
    return pending !== null ? [this.place(pending, null)] : [];
  }

  private place(frame: DecodedFrame, next: DecodedFrame | null): PlacedFrame {
    const t0 = frameStartSeconds(frame);
    let span: number | null = null;
    if (next !== null) {
      const untilNext = frameStartSeconds(next) - t0;
      if (next.header.sequence === ((frame.header.sequence + 1) >>> 0)) {
        span = untilNext;
        this.chunkSeconds = untilNext;
      } else {
        span = this.chunkSeconds !== null ? Math.min(this.chunkSeconds, untilNext) : untilNext;
      }
    } else if (this.endSeconds !== null) {
      const untilEnd = this.endSeconds - t0;
      span = this.chunkSeconds !== null ? Math.min(this.chunkSeconds, untilEnd) : untilEnd;
    }
    return span !== null && span > 0 ? { frame, spanSeconds: span } : { frame };
  }
}

type MutableTrace =
  | { kind: "analog"; channel: number; label: string; t: number[]; v: number[] }
  | { kind: "analog-envelope"; channel: number; label: string; t0: number[]; t1: number[]; vMin: number[]; vMax: number[] }
  | { kind: "digital"; channel: number; label: string; lane: number; segments: DigitalSegment[] };

/**
 * Collects one capture's frames into whole traces. A capture larger than one
 * frame arrives as several (SPEC §3.2), each placed at its own `t0Ns`; drawing
 * only the latest one left a 65 536-sample capture as an empty axis with a
 * sliver at its right edge (B-72).
 *
 * Appending is in place and costs only the new frame: a slow capture streams
 * hundreds of small frames, and copying everything received so far on each of
 * them jammed the page. A `snapshot` still copies the whole capture, so the
 * caller takes one per redraw, not per frame. A trace joins the earlier one of the same kind and
 * channel; a digital level held across a frame boundary stays one segment.
 */
export class TraceAccumulator {
  private readonly traces: MutableTrace[] = [];

  add(next: readonly Trace[]): void {
    for (const trace of next) {
      const earlier = this.traces.find((t) => t.kind === trace.kind && t.channel === trace.channel);
      if (earlier === undefined) this.traces.push(copyTrace(trace));
      else joinInto(earlier, trace);
    }
  }

  /** The traces so far, as copies a renderer may keep while more frames arrive. */
  snapshot(): Trace[] {
    return this.traces.map(copyTrace);
  }
}

function copyTrace(trace: Trace): MutableTrace {
  switch (trace.kind) {
    case "analog":
      return { ...trace, t: trace.t.slice(), v: trace.v.slice() };
    case "analog-envelope":
      return { ...trace, t0: trace.t0.slice(), t1: trace.t1.slice(), vMin: trace.vMin.slice(), vMax: trace.vMax.slice() };
    case "digital":
      return { ...trace, segments: trace.segments.slice() };
  }
}

/** Appends without spreading into a call: a frame can hold more items than a call may take arguments. */
function append<T>(into: T[], from: readonly T[]): void {
  for (const item of from) into.push(item);
}

function joinInto(earlier: MutableTrace, later: Trace): void {
  if (earlier.kind === "analog" && later.kind === "analog") {
    append(earlier.t, later.t);
    append(earlier.v, later.v);
  } else if (earlier.kind === "analog-envelope" && later.kind === "analog-envelope") {
    append(earlier.t0, later.t0);
    append(earlier.t1, later.t1);
    append(earlier.vMin, later.vMin);
    append(earlier.vMax, later.vMax);
  } else if (earlier.kind === "digital" && later.kind === "digital") {
    const last = earlier.segments[earlier.segments.length - 1];
    const first = later.segments[0];
    if (last !== undefined && first !== undefined && last.state === first.state && last.state !== "toggled") {
      earlier.segments[earlier.segments.length - 1] = { t0: last.t0, t1: first.t1, state: last.state };
      append(earlier.segments, later.segments.slice(1));
    } else {
      append(earlier.segments, later.segments);
    }
  }
}
