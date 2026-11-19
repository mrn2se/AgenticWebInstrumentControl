import type { DecodedAnnotation, DecoderId, DecoderInfo, OptionSetting } from "../../proto/types";
import { formatSiDuration } from "../../format";
import { autoRateOptionId } from "../decoder-form";

/**
 * What the Decode panel shows above the frame list once a decode is done: the
 * bit rate it ran at, how many frames and errors, and the data bytes as text
 * or a hex dump - a UART burst read as `Hello\n`, not as 6 rows of `0x48`.
 *
 * Built from the frame-level (level 0) annotations alone, so it needs nothing
 * from the server beyond what a decode already returns. Pure, no DOM.
 */

/** One unit of the data stream: a byte (or a 5-9 bit word), or a frame that carried none because it was broken. */
export type DecodedUnit = { readonly kind: "data"; readonly value: number } | { readonly kind: "error" };

export interface DecodeSummary {
  readonly frames: number;
  readonly units: readonly DecodedUnit[];
  /** How many data units there are (`units` minus the errors). */
  readonly bytes: number;
  readonly framingErrors: number;
  readonly parityErrors: number;
  readonly breaks: number;
  /** Incomplete frames and decoder errors. */
  readonly otherErrors: number;
  /** From the first frame's start to the last frame's end; `null` with no frames. */
  readonly spanSeconds: number | null;
}

/** The rate a decode ran at, for the summary line: set by the user, or found by auto-baud. */
export interface DecodeRate {
  readonly value: number;
  readonly detected: boolean;
}

/**
 * The rate a decode ran at: what auto-baud found, else the number that was
 * sent, else the decoder's default (an option left untouched is not sent).
 * `null` for a decoder with no bit rate to report.
 */
export function decodeRateOf(
  decoder: DecoderInfo,
  options: readonly OptionSetting[],
  detected: number | null,
): DecodeRate | null {
  const id = autoRateOptionId(decoder.id);
  if (id === null) return null;
  if (detected) return { value: detected, detected: true };
  const sent = options.find((o) => o.id === id)?.value;
  if (typeof sent === "number" && sent > 0) return { value: sent, detected: false };
  const spec = decoder.options.find((o) => o.id === id);
  return spec?.type === "int" ? { value: spec.default, detected: false } : null;
}

export function summarizeDecode(annotations: readonly DecodedAnnotation[], rateHz: number): DecodeSummary {
  const frames = annotations.filter((a) => a.level === 0);
  const units: DecodedUnit[] = [];
  let framingErrors = 0;
  let parityErrors = 0;
  let breaks = 0;
  let otherErrors = 0;
  for (const a of frames) {
    switch (a.kind.kind) {
      case "data":
        units.push({ kind: "data", value: a.kind.value });
        break;
      case "framing_error":
        framingErrors += 1;
        units.push({ kind: "error" });
        break;
      case "parity_error":
        parityErrors += 1;
        units.push({ kind: "error" });
        break;
      case "break":
        breaks += 1;
        break;
      case "incomplete":
      case "error":
        otherErrors += 1;
        units.push({ kind: "error" });
        break;
      default:
        break;
    }
  }
  const first = frames[0];
  const last = frames[frames.length - 1];
  const rate = rateHz > 0 ? rateHz : 1;
  return {
    frames: frames.length,
    units,
    bytes: units.filter((u) => u.kind === "data").length,
    framingErrors,
    parityErrors,
    breaks,
    otherErrors,
    spanSeconds: first && last ? (last.end_sample - first.start_sample) / rate : null,
  };
}

/** `9600 baud (detected) · 42 bytes in 43.75 ms · 1 framing error`. */
export function summaryLine(decoder: DecoderId, summary: DecodeSummary, rate: DecodeRate | null): string {
  const parts: string[] = [];
  if (rate) parts.push(`${rate.value} ${decoder === "can" ? "bit/s" : "baud"}${rate.detected ? " (detected)" : ""}`);
  const count = summary.bytes > 0 ? plural(summary.bytes, "byte") : plural(summary.frames, "frame");
  parts.push(summary.spanSeconds !== null && summary.frames > 0 ? `${count} in ${formatSiDuration(summary.spanSeconds)}` : count);
  if (summary.framingErrors) parts.push(plural(summary.framingErrors, "framing error"));
  if (summary.parityErrors) parts.push(plural(summary.parityErrors, "parity error"));
  if (summary.breaks) parts.push(plural(summary.breaks, "break"));
  if (summary.otherErrors) parts.push(plural(summary.otherErrors, "incomplete frame"));
  return parts.join(" · ");
}

function plural(n: number, word: string): string {
  return `${n} ${word}${n === 1 ? "" : "s"}`;
}

/** Stands in for a broken frame in the text view. */
export const ERROR_CHAR = "�";
/** Stands in for a non-printable byte in the text view and the hex dump's ASCII column. */
export const NON_PRINTABLE = "·";

/**
 * The data as text: printable ASCII as is, CR LF / CR / LF as one line break,
 * tab kept, every other byte as `·`, and a broken frame as `�` - so a dropped
 * character shows where it was instead of silently closing up the text.
 */
export function unitsToText(units: readonly DecodedUnit[]): string {
  let out = "";
  for (let i = 0; i < units.length; i++) {
    const u = units[i];
    if (u === undefined) continue;
    if (u.kind === "error") {
      out += ERROR_CHAR;
      continue;
    }
    const v = u.value;
    if (v === 0x0d) {
      out += "\n";
      const next = units[i + 1];
      if (next?.kind === "data" && next.value === 0x0a) i += 1;
    } else if (v === 0x0a || v === 0x09) {
      out += String.fromCharCode(v);
    } else if (v >= 0x20 && v < 0x7f) {
      out += String.fromCharCode(v);
    } else {
      out += NON_PRINTABLE;
    }
  }
  return out;
}

/**
 * A classic hex dump, `perLine` units a row: offset, the values, and their
 * ASCII. A broken frame is `??`. Values wider than a byte (9 data bits) widen
 * every column, so the rows stay aligned.
 */
export function unitsToHexDump(units: readonly DecodedUnit[], perLine = 8): string {
  const wide = units.some((u) => u.kind === "data" && u.value > 0xff);
  const width = wide ? 3 : 2;
  const rows: string[] = [];
  for (let offset = 0; offset < units.length; offset += perLine) {
    const chunk = units.slice(offset, offset + perLine);
    const hex = chunk
      .map((u) => (u.kind === "error" ? "?".repeat(width) : u.value.toString(16).padStart(width, "0")))
      .join(" ")
      .padEnd(perLine * (width + 1) - 1, " ");
    const ascii = chunk
      .map((u) => (u.kind === "error" ? ERROR_CHAR : u.value >= 0x20 && u.value < 0x7f ? String.fromCharCode(u.value) : NON_PRINTABLE))
      .join("");
    rows.push(`${offset.toString(16).padStart(4, "0")}  ${hex}  ${ascii}`);
  }
  return rows.join("\n");
}
