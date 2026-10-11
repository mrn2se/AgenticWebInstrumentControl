import type { ErrorInfo } from "../../proto/types";

/**
 * Client-side state for an in-flight `usb.power` request. This is
 * deliberately *not* a source of truth for whether a port is on — that is
 * `PortRow.power`, which only ever changes when a fresh `usb.tree` result
 * (or, in principle, a future server event) says so. This tracker exists
 * only to render "pending" while a request is outstanding and to hold a
 * guardrail refusal (`port_protected` etc.) long enough to show it
 * next to the control the user just clicked.
 *
 * SPECIFICATION.md §11 / the task brief are explicit: "the server is
 * authoritative... do not flip the toggle optimistically." Nothing in this
 * file is capable of doing that — it has no `on` field at all.
 */
export type PortPowerPhase = "idle" | "pending" | "error";

export interface PortPowerState {
  readonly phase: PortPowerPhase;
  readonly error?: ErrorInfo;
}

const IDLE: PortPowerState = { phase: "idle" };

/** Key for one switchable port: the parent hub's formatted id plus its port number. */
export function portKey(parentHub: string, port: number): string {
  return `${parentHub}:${port}`;
}

export class PortPowerTracker {
  private states = new Map<string, PortPowerState>();

  /** Call right before sending the `usb.power` request. */
  begin(key: string): void {
    this.states.set(key, { phase: "pending" });
  }

  /** Call when the request settles successfully. Does not record an "on" value — see above. */
  resolve(key: string): void {
    this.states.set(key, IDLE);
  }

  /** Call when the request is rejected, including a guardrail refusal from the server. */
  fail(key: string, error: ErrorInfo): void {
    this.states.set(key, { phase: "error", error });
  }

  /** Clears a stale error so the control returns to normal, e.g. before a retry. */
  clear(key: string): void {
    this.states.set(key, IDLE);
  }

  get(key: string): PortPowerState {
    return this.states.get(key) ?? IDLE;
  }
}
