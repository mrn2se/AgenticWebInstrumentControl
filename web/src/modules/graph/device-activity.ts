import type { RecordingSummary } from "../../proto/types";
import { canStart, canStop, type AcquisitionUiState } from "./acquisition";

/**
 * One acquisition state per device, whichever mode is on screen (B-66), and a
 * state for a start that has been sent but not answered (B-67).
 *
 * Single and Live each used to read their own half of the truth. A recording
 * holding the device showed as "recording" in Live and left Single offering a
 * **Start** that the server then refused with `device_busy`, while a recording
 * that had died showed as a bare "error" in Live and stayed "recording" in
 * Single. And for the 50–300 ms between a click on Start and the server's
 * reply, both modes said "idle" with Start still enabled, so a second click
 * sent a second start (the client half of the recorder race, B-59) and a Stop
 * in that window did nothing at all while the run carried on.
 *
 * The rules live here, with no DOM and no socket, so they can be tested: the
 * module reads one answer and renders it.
 */
export type StartPhase = "none" | "starting" | "stopping";

export interface DeviceActivity {
  /** What holds the device. `starting`/`stopping` are this client's own pending request. */
  readonly kind: "idle" | "starting" | "stopping" | "armed" | "running" | "recording" | "stopped" | "error";
  /** The status line's text. */
  readonly label: string;
  /** Why, when there is something to say — an error message, or what holds the device. */
  readonly detail: string | null;
  readonly canStart: boolean;
  readonly canStop: boolean;
}

export interface ActivityInput {
  readonly mode: "single" | "live";
  readonly acq: AcquisitionUiState;
  /** This client's own in-flight start or stop. */
  readonly phase: StartPhase;
  /**
   * The recording on the selected device, in whatever state — including one
   * started by an agent or another tab, which holds the device just the same
   * (SPEC §2.8: recordings are server-owned).
   */
  readonly recording: RecordingSummary | null;
  /**
   * Whether Live mode is already showing `recording`. One it is not showing
   * (left by a closed tab, or an agent's) is what Start attaches to, so Start
   * stays enabled for it in Live mode (B-31). Omitted means watching.
   */
  readonly watching?: boolean;
}

export function deviceActivity(input: ActivityInput): DeviceActivity {
  // This client's own pending request wins: it is the most recent thing known
  // about the device, and it is the window the double-start came through.
  if (input.phase === "starting") {
    return {
      kind: "starting",
      label: "starting…",
      detail: "waiting for the server to answer",
      canStart: false,
      // Stop cancels a start that has not been answered yet: the caller stops
      // it as soon as the reply names the stream or recording.
      canStop: true,
    };
  }
  if (input.phase === "stopping") {
    return { kind: "stopping", label: "stopping…", detail: null, canStart: false, canStop: false };
  }

  const recording = input.recording;
  if (recording?.state === "recording") {
    const attachable = input.mode === "live" && input.watching === false;
    return {
      kind: "recording",
      label: "recording",
      detail:
        input.mode === "single"
          ? `recording ${recording.rec} holds this device — stop it, or switch to Live to watch it`
          : attachable
            ? `recording ${recording.rec} holds this device — Start watches it, Stop ends it`
            : null,
      canStart: attachable,
      canStop: true,
    };
  }
  // A recording that died says so in both modes, with the reason it carries.
  if (recording?.state === "error") {
    return {
      kind: "error",
      label: "error",
      detail: recording.msg ?? `recording ${recording.rec} ended with an error`,
      canStart: true,
      canStop: false,
    };
  }

  const acq = input.acq;
  if (input.mode === "live") {
    // The other direction of B-66, found by the start/stop run: a finite
    // acquisition holds the device exactly as a recording does, and Live read
    // "idle" over one with Start enabled - so a rec.start went out and came
    // back device_busy. One state per device means both directions.
    // A named stream that has not reported yet holds it too; one that has
    // finished on its own (`stopped`/`error`) keeps its id for the capture
    // handle, but holds nothing - counting it left Live's Start disabled
    // after every completed Single capture.
    const streamHolds =
      acq.state === "armed" || acq.state === "running" || (acq.streamId !== null && acq.state === "idle");
    if (streamHolds) {
      return {
        kind: acq.state === "idle" ? "starting" : acq.state,
        label: acq.state === "idle" ? "started" : acq.state,
        detail: "a single capture holds this device — stop it, or watch it in Single",
        canStart: false,
        canStop: true,
      };
    }
    // A stopped recording still on screen is history, not a hold on the device.
    return {
      kind: recording?.state === "stopped" ? "stopped" : "idle",
      label: recording?.state === "stopped" ? "stopped" : "idle",
      detail: null,
      canStart: true,
      canStop: false,
    };
  }

  // A start that has been answered but not yet reported on: the server named a
  // stream, and no `acq.state` event has arrived for it. "idle" with Start
  // enabled is wrong for that - on a local server it is where most of the
  // 50-300 ms of B-67 actually sits, and a second click there still sent a
  // second start. Knowing which stream was started is bookkeeping about what to
  // watch, not a claim about what the device is doing (see `acquisition.ts`).
  if (acq.streamId !== null && acq.state === "idle") {
    return {
      kind: "starting",
      label: "started",
      detail: "waiting for the device to report",
      canStart: false,
      canStop: true,
    };
  }
  return {
    kind: acq.state,
    label: acq.state,
    detail: acq.message,
    canStart: canStart(acq),
    canStop: canStop(acq),
  };
}

/** The status line, as the toolbar shows it: `state` or `state — detail`. */
export function activityLine(activity: DeviceActivity): string {
  return activity.detail ? `${activity.label} — ${activity.detail}` : activity.label;
}

/**
 * What a Stop has to act on: whatever holds the device, not whichever mode is
 * on screen.
 *
 * Both directions were wrong once. B-66 made Single offer Stop over a recording
 * and it reached the stream path, which did nothing; routing by the recording
 * alone then left Stop in Live over a running capture doing nothing either.
 * Both were enabled buttons that did nothing - the thing B-67 is about.
 *
 * Takes the two facts rather than a [`DeviceActivity`], which is a *display*
 * state: a caller that had already entered its "stopping" phase handed that in
 * and the router read it instead of what held the device, so the Stop went
 * nowhere a third time. Facts cannot be shadowed that way.
 */
export function stopTarget(
  holdsRecording: boolean,
  hasStream: boolean,
): "recording" | "stream" | "mode-default" {
  if (holdsRecording) return "recording";
  if (hasStream) return "stream";
  return "mode-default";
}
