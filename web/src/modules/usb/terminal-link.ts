import type { TerminalSourceInfo } from "../../proto/types";

/**
 * Finds the `serial` terminal source belonging to a USB tree row's device
 * (SPEC §11.2.1: "The USB tree shows a terminal icon on a port whose device
 * has a UART"). `TerminalSourceInfo.device` is exactly the tree row's own
 * bus-path key (`tree-model.ts`'s `formatId`) — an exact match, not a guess
 * at `source`'s structure. Returns the first match: a device with more than
 * one serial interface is rare on this bench, and the icon only needs *a*
 * terminal to open, not an exhaustive list of them.
 */
export function serialSourceForDevice(
  sources: readonly TerminalSourceInfo[],
  deviceKey: string,
): TerminalSourceInfo | undefined {
  return sources.find((s) => s.kind === "serial" && s.device === deviceKey);
}
