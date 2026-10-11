import type { PortPowerState } from "./port-power";
import type { PortRow } from "./tree-model";

/**
 * Pure view model for one row's power toggle. Separated from DOM rendering
 * so "does a non-switchable or protected port render disabled with its
 * reason visible" is a plain data assertion, not a DOM query.
 */
export interface ToggleViewModel {
  /** Whether the control may be clicked at all. */
  readonly enabled: boolean;
  /**
   * Why the control is disabled — covers both reasons a port can't be
   * switched (SPEC §4.4): the hub lacks `ppps`, or the port is protected.
   * `null` when `enabled` is true.
   */
  readonly reason: string | null;
  /** A request is outstanding; render a pending state, not a flipped one. */
  readonly pending: boolean;
  /** The server-reported power state. `null` when unknown. Never client-guessed. */
  readonly on: boolean | null;
  /** A guardrail refusal (or other failure) from the last attempt, if any. */
  readonly errorMessage: string | null;
}

export function toggleViewModel(row: PortRow, power: PortPowerState): ToggleViewModel {
  return {
    enabled: row.switchable,
    reason: row.switchable ? null : (row.nonSwitchableReason ?? "this port cannot be switched"),
    pending: power.phase === "pending",
    on: row.power,
    errorMessage: power.phase === "error" && power.error ? `${power.error.code}: ${power.error.msg}` : null,
  };
}
