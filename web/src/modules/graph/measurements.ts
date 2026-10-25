/**
 * Server-side measurements (SPECIFICATION.md §5.4) — "computed server-side
 * in Rust on full-resolution data, never in the browser on decimated data
 * ... otherwise measurements silently degrade as the user zooms out, which
 * is the worst kind of wrong: plausible and unflagged." This module wraps
 * the real `capture.measure` wire op (`awic-proto`'s `MeasurementKind` /
 * `CaptureMeasureResult`, generated into `proto/types.ts`): the browser
 * sends a capture handle, a channel, and the kinds it wants, and renders
 * back exactly what the server says — a value with its unit, or an honest
 * "unavailable" with the server's reason. There is deliberately no branch
 * anywhere in this module that computes a measurement from in-browser data;
 * doing that would be exactly the failure §5.4 exists to prevent.
 */
import type { CaptureMeasureResult, MeasurementKind, MeasurementResult } from "../../proto/types";
import { formatSampleRate, formatSi } from "../../format";

export type { MeasurementKind };

/** What the graph module passes to `WsClient.send("capture.measure", ...)`. */
export type SendCaptureMeasure = (params: {
  capture: string;
  channel: number;
  measurements: MeasurementKind[];
}) => Promise<CaptureMeasureResult>;

export interface MeasurementRequest {
  /** `capture://<uuid>` handle (SPEC §3.1 `acq.start` result) — always full-resolution, never a decimated preview. */
  readonly capture: string;
  readonly channel: number;
  /**
   * The channel's UI label (`D0`, `CH1`, ...), matching `capabilities.ts`'s
   * numbering. Carried alongside the raw wire index so the panel can show
   * "D0" instead of the bare index "0" without re-deriving the
   * analog/digital convention from `channel` alone.
   */
  readonly label: string;
  readonly kinds: readonly MeasurementKind[];
  /**
   * Analog only: volts per ADC code. A voltage is shown to this resolution
   * and no finer — an 8-bit converter on a ±5 V range steps in about 39 mV,
   * and six digits after the point claimed a precision it never had.
   */
  readonly resolutionV?: number;
}

/** One requested kind's outcome, mirroring the wire's `MeasurementResult`. */
export type MeasurementOutcome =
  | { readonly status: "ok"; readonly value: number; readonly unit: string }
  | { readonly status: "unavailable"; readonly reason: string };

export interface MeasurementEntry {
  readonly kind: MeasurementKind;
  readonly outcome: MeasurementOutcome;
}

export type MeasurementStatus =
  | { readonly status: "pending"; readonly request: MeasurementRequest }
  | { readonly status: "ok"; readonly request: MeasurementRequest; readonly entries: readonly MeasurementEntry[] }
  | { readonly status: "error"; readonly request: MeasurementRequest; readonly message: string };

function toEntry(result: MeasurementResult): MeasurementEntry {
  // `MeasurementResult` is untagged on the wire (`{ kind, value, unit }` or
  // `{ kind, unavailable }`); `"value" in result` is how TS narrows it back.
  if ("value" in result) {
    return { kind: result.kind, outcome: { status: "ok", value: result.value, unit: result.unit } };
  }
  return { kind: result.kind, outcome: { status: "unavailable", reason: result.unavailable } };
}

/**
 * Sends one `capture.measure` request and resolves with its outcome. Never
 * throws: a transport/RPC failure comes back as `{ status: "error" }` like
 * any other unsuccessful request, so a caller can render it the same way it
 * renders a server-reported "unavailable".
 */
export async function requestMeasurement(
  send: SendCaptureMeasure,
  request: MeasurementRequest,
): Promise<MeasurementStatus> {
  try {
    const result = await send({
      capture: request.capture,
      channel: request.channel,
      measurements: [...request.kinds],
    });
    return { status: "ok", request, entries: result.results.map(toEntry) };
  } catch (err) {
    return { status: "error", request, message: err instanceof Error ? err.message : String(err) };
  }
}

/** One renderable row: which channel (by its UI label) and kind, and the text to show for it. */
export interface MeasurementRow {
  readonly label: string;
  readonly kind: MeasurementKind;
  readonly text: string;
}

/** Times that come from single edges (one sample either way matters) get 3 digits; periods and frequencies, averaged over many cycles, get 4. */
const THREE_DIGIT_KINDS: ReadonlySet<MeasurementKind> = new Set<MeasurementKind>([
  "rise_time",
  "fall_time",
  "pulse_width_high",
  "pulse_width_low",
]);

/**
 * One measured value as text: SI prefixes the way the time axis uses them
 * (`819 ns`, `1.000 ms`, `1.000 kHz`), volts to the channel's resolution,
 * percentages to a tenth, counts as integers.
 */
export function formatMeasurementValue(
  kind: MeasurementKind,
  value: number,
  unit: string,
  resolutionV?: number,
): string {
  if (!Number.isFinite(value)) return `${value} ${unit}`.trim();
  switch (unit) {
    case "s":
    case "Hz":
      return formatSi(value, unit, THREE_DIGIT_KINDS.has(kind) ? 3 : 4);
    case "V": {
      if (resolutionV !== undefined && resolutionV > 0) {
        const decimals = Math.max(0, Math.min(6, Math.ceil(-Math.log10(resolutionV))));
        return `${value.toFixed(decimals)} V`;
      }
      return formatSi(value, "V", 3);
    }
    case "%":
      return `${value.toFixed(1)} %`;
    case "count":
    case "":
      return Number.isInteger(value) ? String(value) : value.toPrecision(4);
    default:
      return `${Number.isInteger(value) ? String(value) : value.toPrecision(4)} ${unit}`;
  }
}

/**
 * Turns a [`MeasurementStatus`] into the rows a panel renders — one per
 * requested kind, in every case (pending, ok, or a transport error), so the
 * UI never has to special-case "there's nothing to show for this kind".
 * Unavailability is rendered as text, never as a blank cell or a number.
 */
export function measurementRows(status: MeasurementStatus): readonly MeasurementRow[] {
  const label = status.request.label;
  if (status.status === "pending") {
    return status.request.kinds.map((kind) => ({ label, kind, text: "measuring…" }));
  }
  if (status.status === "error") {
    return status.request.kinds.map((kind) => ({ label, kind, text: `error: ${status.message}` }));
  }
  return status.entries.map((entry) => ({
    label,
    kind: entry.kind,
    text:
      entry.outcome.status === "ok"
        ? formatMeasurementValue(entry.kind, entry.outcome.value, entry.outcome.unit, status.request.resolutionV)
        : `unavailable — ${entry.outcome.reason}`,
  }));
}

/** What a block of results was measured on: shown as its header, so rows from different captures never run together. */
export interface CaptureInfo {
  readonly capture: string;
  /** When the capture was taken, ms since the epoch (`Date.now()`). */
  readonly takenAtMs: number;
  readonly rateHz: number;
  readonly samples: number;
  readonly source: "single" | "snapshot";
}

/**
 * How many samples `rec.snapshot` stores for `range`, worked out the way the
 * server does it: each end rounded to a sample, negative times taken as 0,
 * and the range kept inside what the recording still holds — from its first
 * retained sample to its head (`rec.list`'s `first_sample`/`head_sample`;
 * omitted when not known). The visible range of a Live view can reach past
 * either, so its plain duration overstated the snapshot.
 */
export function snapshotSampleCount(
  range: { readonly t0: number; readonly t1: number },
  rateHz: number,
  retained?: { readonly first_sample: number; readonly head_sample: number },
): number {
  if (!(rateHz > 0)) return 0;
  let start = Math.max(0, Math.round(range.t0 * rateHz));
  let end = Math.max(0, Math.round(range.t1 * rateHz));
  if (retained) {
    start = Math.max(start, retained.first_sample);
    end = Math.min(end, retained.head_sample);
  }
  return Math.max(0, end - start);
}

/** How many measurement requests the panel keeps, across all captures. */
const MAX_STATUSES = 20;

/**
 * Adds a new request's status in front. An earlier request for the same
 * channel of the same capture is dropped: measuring again replaces the rows
 * rather than stacking a second copy of them.
 */
export function addMeasurementStatus(
  statuses: readonly MeasurementStatus[],
  next: MeasurementStatus,
): MeasurementStatus[] {
  const kept = statuses.filter(
    (s) => !(s.request.capture === next.request.capture && s.request.channel === next.request.channel),
  );
  return [next, ...kept].slice(0, MAX_STATUSES);
}

export interface MeasurementBlock {
  readonly capture: string;
  readonly statuses: readonly MeasurementStatus[];
}

/**
 * Groups statuses by capture, newest capture first. `takenAt` gives a
 * capture's time when known; blocks are sorted by it, so re-measuring an old
 * capture does not lift its block above the current one. Captures without a
 * time keep the order their results were requested in, after the rest.
 */
export function measurementBlocks(
  statuses: readonly MeasurementStatus[],
  takenAt: (capture: string) => number | undefined = () => undefined,
): MeasurementBlock[] {
  const blocks: { capture: string; statuses: MeasurementStatus[] }[] = [];
  for (const status of statuses) {
    const capture = status.request.capture;
    let block = blocks.find((b) => b.capture === capture);
    if (!block) {
      block = { capture, statuses: [] };
      blocks.push(block);
    }
    block.statuses.push(status);
  }
  // A stable sort: equal keys (two unknown times) keep their order.
  const key = (b: MeasurementBlock): number => takenAt(b.capture) ?? -Infinity;
  return blocks.sort((a, b) => key(b) - key(a));
}

function formatClockTime(ms: number): string {
  const d = new Date(ms);
  const pad = (n: number): string => String(n).padStart(2, "0");
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

/** A block's header: `Capture 14:03:22 · 1 MS/s · 10 000 samples` (or `Snapshot …`). */
export function captureHeader(info: CaptureInfo | undefined): string {
  if (!info) return "Capture";
  const what = info.source === "snapshot" ? "Snapshot" : "Capture";
  const samples = Math.round(info.samples).toLocaleString("en-US").replace(/,/g, "\u202f");
  return `${what} ${formatClockTime(info.takenAtMs)} · ${formatSampleRate(info.rateHz)} · ${samples} samples`;
}

/** The kinds offered by the panel's "Measure" button for an analog channel. */
export const ANALOG_MEASUREMENT_KINDS: readonly MeasurementKind[] = [
  "vpp",
  "vmin",
  "vmax",
  "vavg",
  "vrms",
  "frequency",
  "period",
  "duty",
  "rise_time",
  "fall_time",
];

/** The kinds offered by the panel's "Measure" button for a digital channel. */
export const DIGITAL_MEASUREMENT_KINDS: readonly MeasurementKind[] = [
  "frequency",
  "period",
  "duty",
  "pulse_width_high",
  "pulse_width_low",
  "edge_count",
];
