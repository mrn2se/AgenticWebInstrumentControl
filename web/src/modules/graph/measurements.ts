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

function formatValue(value: number): string {
  return Number.isInteger(value) ? String(value) : value.toPrecision(6);
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
        ? `${formatValue(entry.outcome.value)} ${entry.outcome.unit}`.trim()
        : `unavailable — ${entry.outcome.reason}`,
  }));
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
