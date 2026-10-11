import type { DeviceInfo } from "../../proto/types";

/** What the device picker shows selected after the device list changed. */
export interface DeviceSelection {
  /** The selected device's id, or null when there is none at all. */
  readonly id: string | null;
  /** What the controls are built from: the device as on the bus now, or as
   *  last seen while it is gone. */
  readonly info: DeviceInfo | null;
  /** The selected device is known but not on the bus right now. */
  readonly departed: boolean;
}

/**
 * Keeps the user's device selected across the list changing. A device that
 * leaves the bus (port power cut, unplugged, re-flashing) stays selected as
 * `departed`, built from `lastSeen`, rather than the picker jumping to the
 * first other instrument — which pointed the next Start at the wrong device
 * and rebuilt the controls, losing the rate and ranges. Only with no prior
 * selection does it fall back to the first device.
 */
export function chooseSelection(
  devices: readonly DeviceInfo[],
  previousId: string | null,
  lastSeen: DeviceInfo | null,
): DeviceSelection {
  const present = previousId !== null ? devices.find((d) => d.id === previousId) : undefined;
  if (present) return { id: present.id, info: present, departed: false };
  if (previousId !== null && lastSeen?.id === previousId) return { id: previousId, info: lastSeen, departed: true };
  const first = devices[0] ?? null;
  return { id: first?.id ?? null, info: first, departed: false };
}
