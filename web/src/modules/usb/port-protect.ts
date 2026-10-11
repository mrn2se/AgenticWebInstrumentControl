import type { ErrorInfo } from "../../proto/types";

/**
 * Client-side state for an in-flight `usb.protect` request — the lock
 * control's counterpart to `port-power.ts`'s `PortPowerTracker`, tracked
 * separately so a pending power toggle and a pending lock change on the same
 * port never stomp each other's "pending" rendering.
 *
 * Exactly the same discipline as the power tracker: this is never a source
 * of truth for a port's protection state. That is `PortRow.protected` /
 * `PortRow.userChoice`, which only change when a fresh `usb.tree` result
 * says so (the server publishes `usb.changed` with change `"protection"`;
 * the USB module re-fetches on it like every other tree-affecting event).
 * This tracker only renders "pending" while a request is outstanding and
 * holds a guardrail refusal (`port_protected`, `state_write_failed`, ...)
 * long enough to show it next to the control the user just clicked.
 */
export type PortProtectPhase = "idle" | "pending" | "error";

export interface PortProtectState {
  readonly phase: PortProtectPhase;
  readonly error?: ErrorInfo;
}

const IDLE: PortProtectState = { phase: "idle" };

export class PortProtectTracker {
  private states = new Map<string, PortProtectState>();

  /** Call right before sending the `usb.protect` request. */
  begin(key: string): void {
    this.states.set(key, { phase: "pending" });
  }

  /** Call when the request settles successfully. Does not record a lock value — see above. */
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

  get(key: string): PortProtectState {
    return this.states.get(key) ?? IDLE;
  }
}
