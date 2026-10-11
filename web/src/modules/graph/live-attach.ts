import type { RecordingSummary } from "../../proto/types";

/**
 * Live mode's starting rule (SPECIFICATION.md §5.5, §2.8: "recordings are
 * server-owned and shared with agents"): if `rec.list` already shows a
 * recording for the selected device, attach to it instead of calling
 * `rec.start` again — a second `rec.start` on a recording device is refused
 * by the server anyway (`device_busy`), but the UI should never even try;
 * an agent may already be watching this exact recording.
 *
 * A `stopped`/`error` recording for the device is not attachable — `rec.start`
 * on that device is not refused (the old entry just sits there, inert), so
 * the right move is a fresh recording, not silently viewing a dead one.
 */
export function pickRecordingToAttach(
  recordings: readonly RecordingSummary[],
  device: string,
): RecordingSummary | undefined {
  return recordings.find((r) => r.device === device && r.state === "recording");
}

/**
 * Whether a Live run started its own recording, and so owns the cleanup when it
 * stops (B-68), or attached to one that was already there — an agent's, or
 * another tab's, which belongs to whoever started it (SPEC §2.8).
 *
 * A function rather than a comparison at the call site because the comparison
 * was wrong: [`pickRecordingToAttach`] is an `Array.find`, so it answers
 * `undefined`, and `existing === null` was false whether or not anything had
 * been attached to. The tab therefore never deleted a recording it had started,
 * which is the whole of B-68.
 */
export function startedItsOwnRecording(attached: RecordingSummary | undefined): boolean {
  return attached === undefined;
}
