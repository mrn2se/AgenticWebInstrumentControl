import type { AcqState } from "../../proto/types";

/**
 * Local view of one acquisition stream's lifecycle. SPECIFICATION.md §11:
 * "the server is authoritative and the client never maintains a parallel
 * truth" — so this never guesses `state` from a request having been sent;
 * `state` only ever changes in response to a real `acq.state` event
 * (`applyAcqStateEvent`). Calling `acq.start` does let the module learn
 * *which* stream id to start listening for (`applyStreamStarted`), which is
 * bookkeeping about what to watch, not a claim about what the device is
 * doing.
 */
export interface AcquisitionUiState {
  readonly streamId: number | null;
  readonly captureHandle: string | null;
  /** `"idle"` until the server's first `acq.state` event for this stream arrives. */
  readonly state: AcqState | "idle";
  readonly message: string | null;
}

export function initialAcquisitionState(): AcquisitionUiState {
  return { streamId: null, captureHandle: null, state: "idle", message: null };
}

/** Records the stream id and capture handle an `acq.start` response named — not an assumed running state. */
export function applyStreamStarted(
  state: AcquisitionUiState,
  started: { readonly stream: number; readonly capture: string },
): AcquisitionUiState {
  return { ...state, streamId: started.stream, captureHandle: started.capture, state: "idle", message: null };
}

/** Applies a server-pushed `acq.state` event (SPEC §3.1) — the only source of truth for `state`. */
export function applyAcqStateEvent(
  state: AcquisitionUiState,
  event: { readonly stream: number; readonly state: AcqState; readonly msg?: string | null },
): AcquisitionUiState {
  if (event.stream !== state.streamId) return state; // not this panel's stream
  return { ...state, state: event.state, message: event.msg ?? null };
}

/** Forgets the stream (e.g. after `acq.stop` resolves, or the device disconnects). */
export function applyStopped(state: AcquisitionUiState): AcquisitionUiState {
  return { ...state, streamId: null, captureHandle: null, state: "idle" };
}

export function canStart(state: AcquisitionUiState): boolean {
  return state.state === "idle" || state.state === "stopped" || state.state === "error";
}

export function canStop(state: AcquisitionUiState): boolean {
  return state.state === "armed" || state.state === "running";
}
