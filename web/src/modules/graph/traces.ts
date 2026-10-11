import type { DecodedFrame } from "../../ws/frame";
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
 * bucket-duration field; this assumes (true for this project's single-shot
 * triggered captures, SPEC §2.7) that one frame's buckets tile the whole
 * configured capture evenly, so bucket duration is `(totalSamples/rateHz) /
 * bucketCount`. A streaming (non-single-shot) capture would need the wire
 * format to carry duration explicitly instead.
 */
export interface TimebaseContext {
  readonly rateHz: number;
  /** Total configured samples per channel for the whole capture (`AcqConfig.samples`). */
  readonly totalSamples: number;
}

function bucketDurationSeconds(ctx: TimebaseContext, bucketCount: number): number {
  if (ctx.rateHz <= 0 || bucketCount <= 0) return 0;
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
      return splitDigitalChannels(data).map(
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
      return splitBitwiseChannels(data).map(
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
