import type { AnnotationLane, AnnotationBox } from "../../render/types";
import type { DecodedAnnotation } from "../../proto/types";
import { formatSiDuration } from "../../format";

/**
 * Turns a decode result into what the side panel renders (SPECIFICATION.md
 * T21: "a list of decoded frames in the side panel. Clicking one centers the
 * view on it") and into the renderer's annotation-lane shape — the same
 * split `search-ui.ts` makes between "what the server said" and "what the
 * panel shows for it".
 *
 * `originSeconds` is where the decoded span starts on the plot's own axis, and
 * every time here is on that axis. A decoder counts samples from the start of
 * what it was given, while the plot places a capture at the stream time its
 * frames carry (`t0Ns`): the frame list read `256.000 µs` for a frame the plot
 * drew at 6.6 ms, so a row could not be found in the picture (B-71).
 */

export interface DecodedFrameRow {
  readonly index: number;
  readonly tSeconds: number;
  readonly label: string;
  readonly startSample: number;
  readonly endSample: number;
}

/**
 * One row per **frame**-level annotation (SPEC §6: level 0 is "a whole
 * protocol unit"). Field/bit annotations (levels 1/2) are drawn in the
 * graph's stacked lanes but not separately listed here — exactly how a
 * logic analyzer's decode table shows one row per transaction, not one per
 * clocked bit.
 */
export function decodedFrameRows(
  annotations: readonly DecodedAnnotation[],
  rateHz: number,
  originSeconds = 0,
): DecodedFrameRow[] {
  const rate = rateHz > 0 ? rateHz : 1;
  return annotations
    .filter((a) => a.level === 0)
    .map((a, index) => ({
      index,
      tSeconds: originSeconds + a.start_sample / rate,
      label: a.text,
      startSample: a.start_sample,
      endSample: a.end_sample,
    }));
}

export function formatDecodedFrameTimestamp(row: DecodedFrameRow): string {
  return formatSiDuration(row.tSeconds);
}

/**
 * Turns a flat decode result into the single-channel `AnnotationLane` the
 * `WaveformRenderer` interface takes (SPEC §6, T21) — every annotation came
 * from decoding one role bound to one hardware channel, so they all sit
 * under that one channel's lane; a caller with several simultaneously
 * decoded channels calls this once per channel and concatenates the lanes.
 */
export function annotationLaneFor(
  channel: number,
  annotations: readonly DecodedAnnotation[],
  rateHz: number,
  originSeconds = 0,
): AnnotationLane {
  const rate = rateHz > 0 ? rateHz : 1;
  const boxes: AnnotationBox[] = annotations.map((a) => ({
    t0: originSeconds + a.start_sample / rate,
    t1: originSeconds + a.end_sample / rate,
    level: a.level,
    label: a.text,
  }));
  return { channel, boxes };
}
