import type { RecChannel } from "../../proto/types";
import type { DecodedViewFrame } from "../../ws/view-frame";
import type { AnalogEnvelopeTrace, AnalogTrace, DigitalSegment, DigitalTrace, Trace } from "../../render/types";
import { decodeViewFramePayload } from "./view-decode";
import { codeToUnits } from "./scale";

/**
 * Turns a decoded `0xA6` view frame into renderer-ready `Trace`s (SPEC
 * §5.5's task instructions): "runs -> exact digital segments; digital
 * summary -> toggled or level segments; analog raw -> volts via the
 * recording's calibration; analog summary -> envelope." This is the Live
 * mode counterpart of `traces.ts`; the two are separate modules because a
 * view frame's timing is anchored to an absolute sample index
 * (`firstSample`) rather than a capture's `t0Ns` + total sample count.
 *
 * Each call allocates arrays sized exactly to the frame's `item_count` (via
 * `new Array(n)`, never `.push` in a growing loop) — the same "size once,
 * fill in place" shape `decode.ts` already uses for `0xA5` frames — so a
 * frame never leaves behind more garbage than its own size, even though a
 * fresh array per channel per frame is allocated (SPEC's "reuse buffers
 * where you can" is satisfied by *not* accumulating history here at all:
 * the caller replaces the previous frame's traces wholesale, exactly as
 * `GraphModule` already does for `0xA5` frames via `renderer.setTraces()`).
 */
export interface ViewTraceContext {
  readonly rateHz: number;
  /** The recording's channel list, in the order `rec.start`/`rec.list` report it — also the lane order for digital channels. */
  readonly channels: readonly RecChannel[];
}

function isDigitalName(name: string): boolean {
  return name.startsWith("D");
}

/** Absolute sample index (as a `number`) of item `i`, `samplesPerItem` samples wide, starting at `firstSample`. */
function itemStartSeconds(firstSample: bigint, samplesPerItem: bigint, i: number, rateHz: number): number {
  if (rateHz <= 0) return 0;
  return (Number(firstSample) + i * Number(samplesPerItem)) / rateHz;
}

function digitalRunsToTraces(frame: DecodedViewFrame, ctx: ViewTraceContext): DigitalTrace[] {
  const digitalChannels = ctx.channels.filter((c) => isDigitalName(c.name));
  const data = decodeViewFramePayload(frame);
  if (data.kind !== "digital-runs") throw new Error("digitalRunsToTraces: frame is not digital-runs");
  const dt = ctx.rateHz > 0 ? 1 / ctx.rateHz : 0;

  const segmentsByChannel = new Map<number, DigitalSegment[]>();
  for (const c of digitalChannels) segmentsByChannel.set(c.channel, []);

  let sampleIndex = Number(frame.header.firstSample);
  for (const run of data.runs) {
    const t0 = sampleIndex * dt;
    const t1 = (sampleIndex + run.length) * dt;
    for (const c of digitalChannels) {
      const state: "low" | "high" = ((run.value >> c.channel) & 1) === 1 ? "high" : "low";
      const segments = segmentsByChannel.get(c.channel);
      if (!segments) continue;
      const last = segments[segments.length - 1];
      // A packed-byte run boundary can be caused by a bit this channel
      // doesn't own changing; merge into the previous segment when this
      // channel's own state didn't actually change, exactly as
      // `traces.ts`'s `runLengthSegments` merges equal-state buckets.
      if (last && last.state === state && last.t1 === t0) {
        segments[segments.length - 1] = { t0: last.t0, t1, state };
      } else {
        segments.push({ t0, t1, state });
      }
    }
    sampleIndex += run.length;
  }

  return digitalChannels.map(
    (c, lane): DigitalTrace => ({
      kind: "digital",
      channel: c.channel,
      label: c.name,
      lane,
      segments: segmentsByChannel.get(c.channel) ?? [],
    }),
  );
}

function digitalSummaryToTraces(frame: DecodedViewFrame, ctx: ViewTraceContext): DigitalTrace[] {
  const digitalChannels = ctx.channels.filter((c) => isDigitalName(c.name));
  const data = decodeViewFramePayload(frame);
  if (data.kind !== "digital-summary") throw new Error("digitalSummaryToTraces: frame is not digital-summary");

  return digitalChannels.map((c, lane): DigitalTrace => {
    const segments: DigitalSegment[] = new Array(data.items.length);
    for (let i = 0; i < data.items.length; i++) {
      const item = data.items[i]!;
      const andBit = (item.and >> c.channel) & 1;
      const orBit = (item.or >> c.channel) & 1;
      const state: "low" | "high" | "toggled" = andBit !== orBit ? "toggled" : andBit === 1 ? "high" : "low";
      const t0 = itemStartSeconds(frame.header.firstSample, frame.header.samplesPerItem, i, ctx.rateHz);
      const t1 = itemStartSeconds(frame.header.firstSample, frame.header.samplesPerItem, i + 1, ctx.rateHz);
      segments[i] = { t0, t1, state };
    }
    return { kind: "digital", channel: c.channel, label: c.name, lane, segments };
  });
}

function analogRawToTraces(frame: DecodedViewFrame, ctx: ViewTraceContext): AnalogTrace[] {
  const data = decodeViewFramePayload(frame);
  if (data.kind !== "analog-raw") throw new Error("analogRawToTraces: frame is not analog-raw");
  const dt = ctx.rateHz > 0 ? 1 / ctx.rateHz : 0;
  const t0 = Number(frame.header.firstSample) * dt;

  return data.channels.map((channel): AnalogTrace => {
    const info = ctx.channels[channel.channel];
    const cal = info?.calibration ?? { channel: channel.channel, scale: 1, offset: 0, unit: "" };
    const t: number[] = new Array(channel.codes.length);
    const v: number[] = new Array(channel.codes.length);
    for (let i = 0; i < channel.codes.length; i++) {
      t[i] = t0 + i * dt;
      v[i] = codeToUnits(channel.codes[i]!, cal);
    }
    return { kind: "analog", channel: channel.channel, label: info?.name ?? `CH${channel.channel + 1}`, t, v };
  });
}

function analogSummaryToTraces(frame: DecodedViewFrame, ctx: ViewTraceContext): AnalogEnvelopeTrace[] {
  const data = decodeViewFramePayload(frame);
  if (data.kind !== "analog-summary") throw new Error("analogSummaryToTraces: frame is not analog-summary");

  return data.channels.map((channel): AnalogEnvelopeTrace => {
    const info = ctx.channels[channel.channel];
    const cal = info?.calibration ?? { channel: channel.channel, scale: 1, offset: 0, unit: "" };
    const n = channel.min.length;
    const t0: number[] = new Array(n);
    const t1: number[] = new Array(n);
    const vMin: number[] = new Array(n);
    const vMax: number[] = new Array(n);
    for (let i = 0; i < n; i++) {
      t0[i] = itemStartSeconds(frame.header.firstSample, frame.header.samplesPerItem, i, ctx.rateHz);
      t1[i] = itemStartSeconds(frame.header.firstSample, frame.header.samplesPerItem, i + 1, ctx.rateHz);
      vMin[i] = codeToUnits(channel.min[i]!, cal);
      vMax[i] = codeToUnits(channel.max[i]!, cal);
    }
    return { kind: "analog-envelope", channel: channel.channel, label: info?.name ?? `CH${channel.channel + 1}`, t0, t1, vMin, vMax };
  });
}

/**
 * The exact time span a set of traces just built actually covers. Derived
 * from the data itself, not from `header.itemCount * header.samplesPerItem`
 * (an earlier version computed it that way): a `digital-runs` item is one
 * *run* of variable sample length, not a fixed-size bucket like every other
 * view-frame kind, so `itemCount * samplesPerItem` (`samplesPerItem` is
 * fixed at 1 for runs, SPEC §3.4) drastically understates its span — on
 * real hardware this collapsed the viewport to a handful of microseconds
 * and made every digital lane render off-screen. A following view's frames
 * slide forward as the recording grows, and this is what a caller uses to
 * slide the renderer's viewport to match (SPEC §5.5: "a follow view ... at
 * up to 30 fps") — a renderer only auto-fits *once*, on its first frame
 * (`WaveformRenderer`'s own doc comment), so Live mode must drive
 * `setViewport()` itself on every frame while following or the plot would
 * freeze at its initial window while the underlying data kept moving.
 */
export function tracesTimeBounds(traces: readonly Trace[]): { readonly t0: number; readonly t1: number } | null {
  let min = Infinity;
  let max = -Infinity;
  for (const trace of traces) {
    switch (trace.kind) {
      case "analog":
        for (const t of trace.t) {
          if (t < min) min = t;
          if (t > max) max = t;
        }
        break;
      case "analog-envelope":
        for (const t of trace.t0) if (t < min) min = t;
        for (const t of trace.t1) if (t > max) max = t;
        break;
      case "digital":
        for (const s of trace.segments) {
          if (s.t0 < min) min = s.t0;
          if (s.t1 > max) max = s.t1;
        }
        break;
      default: {
        const exhaustive: never = trace;
        throw new Error(`tracesTimeBounds: unhandled trace kind ${JSON.stringify(exhaustive)}`);
      }
    }
  }
  if (!Number.isFinite(min) || !Number.isFinite(max)) return null;
  return { t0: min, t1: max };
}

/** Dispatches on `frame.header.kind` to the four mappings above. */
export function viewFrameToTraces(frame: DecodedViewFrame, ctx: ViewTraceContext): Trace[] {
  switch (frame.header.kind) {
    case "digital-runs":
      return digitalRunsToTraces(frame, ctx);
    case "digital-summary":
      return digitalSummaryToTraces(frame, ctx);
    case "analog-raw":
      return analogRawToTraces(frame, ctx);
    case "analog-summary":
      return analogSummaryToTraces(frame, ctx);
    default: {
      const exhaustive: never = frame.header.kind;
      throw new Error(`viewFrameToTraces: unhandled kind ${JSON.stringify(exhaustive)}`);
    }
  }
}
