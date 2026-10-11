import type { RecListResult, RecordingSummary } from "../../proto/types";

/**
 * Formats a recording's toolbar status line (SPEC §5.5: "the toolbar shows
 * the retained span and the memory in use"; §11.3: "Status (connection,
 * recording span, memory) is compact text in the header, not a panel.")
 * Pure text formatting, fed by a periodic `rec.list` refresh and `rec.state`
 * events — this module has no opinion on how often that happens.
 */

/** Retained span in seconds: `(head_sample - first_sample) / rate_hz`. */
export function retainedSeconds(summary: RecordingSummary): number {
  if (summary.rate_hz <= 0) return 0;
  return (summary.head_sample - summary.first_sample) / summary.rate_hz;
}

/**
 * The recording's current head, in seconds since its start — the "now" edge
 * of a following view. Used to approximate the visible range for a
 * Snapshot taken while following (SPEC §5.5), since a follow view's exact
 * server-side window isn't otherwise known to the client between frames.
 */
export function headSeconds(summary: RecordingSummary): number {
  if (summary.rate_hz <= 0) return 0;
  return summary.head_sample / summary.rate_hz;
}

function formatSeconds(seconds: number): string {
  if (seconds < 1) return `${(seconds * 1000).toFixed(0)} ms`;
  if (seconds < 60) return `${seconds.toFixed(1)} s`;
  const minutes = Math.floor(seconds / 60);
  const rest = seconds - minutes * 60;
  return `${minutes}m ${rest.toFixed(0)}s`;
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/**
 * One line: retained span, memory used against the recorder's budget
 * (`rec.list`'s `budget_mb`/`used_bytes`, SPEC §2.8.4), and the compression
 * ratio. Deliberately omits the plain recording state (`recording` /
 * `stopped`) — the toolbar's colored state badge already shows that right
 * next to this line, and repeating it read as "recording · recording · ...".
 * An error's own message has nowhere else to show, so it
 * still leads this line when there is one.
 */
export function formatRecordingStatus(summary: RecordingSummary, list: RecListResult): string {
  const span = formatSeconds(retainedSeconds(summary));
  const used = formatBytes(list.used_bytes);
  const budget = `${list.budget_mb} MB`;
  const detail = `${span} retained · ${used} / ${budget} · ${summary.ratio.toFixed(1)}x compression`;
  return summary.state === "error" && summary.msg ? `error — ${summary.msg} · ${detail}` : detail;
}

/** No recording yet (before the first `rec.start`/attach) — a distinct, honest message rather than blank text. */
export const NO_RECORDING_STATUS = "not recording";
