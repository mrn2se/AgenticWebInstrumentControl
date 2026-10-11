import type { Parity, SerialSettings, StopBits } from "../../proto/types";

/** Baud presets offered in the settings popover (task brief §6); "custom"
 * is a separate UI affordance in `index.ts`, not a member of this list. */
export const BAUD_PRESETS: readonly number[] = [
  9600, 19200, 38400, 57600, 115200, 230400, 460800, 921600,
];

export const DEFAULT_SERIAL_SETTINGS: SerialSettings = {
  baud: 115200,
  data_bits: 8,
  parity: "none",
  stop_bits: "one",
};

export const PARITY_OPTIONS: readonly Parity[] = ["none", "even", "odd"];
export const DATA_BITS_OPTIONS: readonly number[] = [5, 6, 7, 8];
export const STOP_BITS_OPTIONS: readonly StopBits[] = ["one", "two"];

/** A baud rate typed into the "custom" field must be a sane positive
 * integer — the UI's own sanity check, ahead of whatever the server (or the
 * kernel's termios) would separately refuse. */
export function isValidCustomBaud(value: number): boolean {
  return Number.isInteger(value) && value > 0 && value <= 4_000_000;
}
