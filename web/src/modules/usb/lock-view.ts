import type { PortProtectState } from "./port-protect";
import type { PortRow } from "./tree-model";

/**
 * Pure view model for one power-switchable port's lock control (SPEC §4.4,
 * §11; task brief §2). Every power-switchable port gets one, independent of
 * whether it happens to be protected *right now* — the user can lock a
 * currently-unprotected port (e.g. a programmer they don't want cut) just as
 * well as unlock a protected one.
 *
 * Three states, mapped straight from `UsbPort.user_choice`:
 *   - `user_choice: true`  -> "locked"    (`usb.protect` sends `protected: true`)
 *   - `user_choice: false` -> "unlocked"  (`usb.protect` sends `protected: false`)
 *   - absent               -> "automatic" (`usb.protect` sends `protected: null`)
 */
export type LockChoice = "automatic" | "locked" | "unlocked";

/** Maps a port's `user_choice` (via `PortRow.userChoice`) to its lock control's current state. */
export function lockChoice(userChoice: boolean | null): LockChoice {
  if (userChoice === true) return "locked";
  if (userChoice === false) return "unlocked";
  return "automatic";
}

/** The `protected` field to send on `usb.protect` for a chosen lock state. */
export function protectedValueForChoice(choice: LockChoice): boolean | null {
  switch (choice) {
    case "locked":
      return true;
    case "unlocked":
      return false;
    case "automatic":
      return null;
  }
}

/**
 * Whether choosing `target` for this port's lock must be confirmed before
 * sending `usb.protect`. True only for switching *to* "unlocked" a port that
 * is protected *automatically* right now — no `user_choice` on record, and a
 * derived protection reason is in force (a hub, HID, disk, or wireless
 * adapter is attached, SPEC §4.4). Locking never needs confirmation
 * (whatever the port's current state), and neither does unlocking a port
 * that is not currently protected, or one the user already locked
 * themselves — reversing your own earlier choice is not the one-click hazard
 * this guards against.
 *
 * This is deliberately the only guardrail standing between one click and
 * cutting a hub or keyboard from the UI (task brief §2) — one click must not
 * remove it.
 */
export function lockChangeNeedsConfirmation(
  row: Pick<PortRow, "derivedProtection" | "userChoice">,
  target: LockChoice,
): boolean {
  // Keyed on the rule's verdict, not on the current state: an earlier version
  // asked only when the port was on "automatic", so locking a hub port first
  // and then unlocking it removed the protection with one unconfirmed click.
  // Already unlocked: nothing changes, nothing to confirm.
  return target === "unlocked" && row.derivedProtection !== null && row.userChoice !== false;
}

export interface LockViewModel {
  /** The port's current lock state, from `user_choice`. */
  readonly choice: LockChoice;
  /** Whether switching is refused right now (SPEC §4.4). */
  readonly protected: boolean;
  /** Why, when protected — shown visibly next to the control, not only in a tooltip. */
  readonly protectionReason: string | null;
  /** A `usb.protect` request is outstanding; render pending, not a flipped state. */
  readonly pending: boolean;
  /** A guardrail refusal (or other failure) from the last attempt, if any. */
  readonly errorMessage: string | null;
}

export function lockViewModel(row: PortRow, state: PortProtectState): LockViewModel {
  return {
    choice: lockChoice(row.userChoice),
    protected: row.protected,
    protectionReason: row.protection,
    pending: state.phase === "pending",
    errorMessage: state.phase === "error" && state.error ? `${state.error.code}: ${state.error.msg}` : null,
  };
}
