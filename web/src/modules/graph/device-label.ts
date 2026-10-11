import type { DeviceInfo } from "../../proto/types";

/**
 * SPECIFICATION.md §5.1: the oscilloscope's physical mode switch changes its USB
 * product id, and in logic mode it enumerates as `0925:3881` — identical to
 * the standalone fx2lafw logic analyzer, with neither device reporting a serial
 * number. "VID:PID is not a unique key on this bench." Devices are
 * identified, and must be *displayed*, by bus path (`DeviceInfo.id`), never
 * by model name — otherwise two `0925:3881` devices become indistinguishable
 * in the UI, which is exactly the "phantom second logic analyzer" incident SPEC
 * §5.1 records.
 */
export function deviceDisplayLabel(device: DeviceInfo): string {
  return device.label ?? device.id;
}

/**
 * Per R-03 (SPEC §5.1): the oscilloscope is presented as a scope, but a device
 * found at `0925:3881` gets a plain disclosure rather than being silently
 * treated as a generic logic analyzer.
 */
export function collisionNote(vid: number, pid: number): string | null {
  return vid === 0x0925 && pid === 0x3881
    ? "An oscilloscope in logic mode, or an fx2lafw logic analyzer; flip the switch for scope"
    : null;
}
