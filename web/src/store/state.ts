import type { DeviceInfo, ErrorInfo, Event as ServerEvent } from "../proto/types";
import type { ConnectionState } from "../ws/client";
import type { FrameGapNotice } from "../ws/sequence";

/**
 * Application state. This mirrors what the server has told the client —
 * `device.added`/`removed`/`state` events, `acq.state` events, and query
 * results — and nothing else. SPECIFICATION.md §11: "the client never
 * maintains a parallel truth." There is deliberately no code path here that
 * mutates `devices` in response to a request being *sent*; only server
 * responses and events reach these reducers (wired in `store/wire.ts`).
 */

export interface ErrorEntry extends ErrorInfo {
  /** Local identity for dismissal; the wire protocol has no id for these. */
  readonly seq: number;
  readonly at: number;
}

export interface StreamStats {
  readonly streamId: number;
  readonly lastSequence: number;
  readonly droppedTotal: number;
}

export interface AppState {
  readonly connection: ConnectionState;
  readonly devices: readonly DeviceInfo[];
  readonly errors: readonly ErrorEntry[];
  readonly streams: Readonly<Record<number, StreamStats>>;
}

export function createInitialState(): AppState {
  return { connection: "connecting", devices: [], errors: [], streams: {} };
}

/** Bounds how much error history the UI keeps. */
const MAX_ERRORS = 50;

let errorSeq = 0;

export function pushError(state: AppState, info: ErrorInfo): AppState {
  const entry: ErrorEntry = { ...info, seq: errorSeq++, at: Date.now() };
  const errors = [...state.errors, entry].slice(-MAX_ERRORS);
  return { ...state, errors };
}

export function dismissError(state: AppState, seq: number): AppState {
  return { ...state, errors: state.errors.filter((e) => e.seq !== seq) };
}

export function setConnectionState(state: AppState, connection: ConnectionState): AppState {
  return { ...state, connection };
}

export function setDevices(state: AppState, devices: readonly DeviceInfo[]): AppState {
  return { ...state, devices: [...devices] };
}

export function recordFrameGap(state: AppState, gap: FrameGapNotice): AppState {
  const prev = state.streams[gap.streamId];
  const droppedTotal = (prev?.droppedTotal ?? 0) + gap.droppedCount;
  const streams = {
    ...state.streams,
    [gap.streamId]: { streamId: gap.streamId, lastSequence: gap.actualSequence, droppedTotal },
  };
  return { ...state, streams };
}

/** Applies one server-pushed event to the state (SPECIFICATION.md §3.1). */
export function applyServerEvent(state: AppState, event: ServerEvent): AppState {
  switch (event.event) {
    case "device.added":
      return { ...state, devices: upsertDevice(state.devices, event.device) };

    case "device.removed":
      return { ...state, devices: state.devices.filter((d) => d.id !== event.device) };

    case "device.state":
      return {
        ...state,
        devices: state.devices.map((d) => (d.id === event.device ? { ...d, state: event.state } : d)),
      };

    case "acq.state":
      // T9 owns per-stream acquisition state; T7 only surfaces failures so
      // they're visible before that module exists.
      if (event.state === "error") {
        return pushError(state, {
          code: "acq_error",
          msg: event.msg ?? `stream ${event.stream} entered error state`,
        });
      }
      return state;

    case "usb.changed":
      // The physical bus, not the instrument registry this store holds. The
      // USB module re-fetches `usb.tree` on this event itself; an instrument
      // that appears as a result arrives separately as `device.added`.
      return state;

    case "rec.state":
      // T18 (recorder wire plumbing) only decodes the frames/types; the
      // live-mode UI that surfaces recording state (T19) hasn't landed yet.
      // A recording ending in error is at least visible as an error entry,
      // same treatment as `acq.state` above.
      if (event.state === "error") {
        return pushError(state, {
          code: "rec_error",
          msg: event.msg ?? `recording ${event.rec} entered error state`,
        });
      }
      return state;

    case "term.state":
    case "term.sources":
    case "mcp.activity":
      // The terminal module owns its own session/source view model and
      // subscribes to `ctx.ws.events` directly (same shape as `usb.changed`
      // above) rather than through this shared store — there is no global
      // state here for either event to update. The MCP tab does the same
      // with `mcp.activity`.
      return state;

    default: {
      const exhaustive: never = event;
      return exhaustive;
    }
  }
}

function upsertDevice(devices: readonly DeviceInfo[], device: DeviceInfo): DeviceInfo[] {
  const idx = devices.findIndex((d) => d.id === device.id);
  if (idx === -1) return [...devices, device];
  const next = [...devices];
  next[idx] = device;
  return next;
}
