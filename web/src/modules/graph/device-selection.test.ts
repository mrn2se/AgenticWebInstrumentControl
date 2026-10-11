import { describe, expect, it } from "vitest";
import type { DeviceInfo } from "../../proto/types";
import { chooseSelection } from "./device-selection";

function device(id: string): DeviceInfo {
  return { id, vid: 0x04b5, pid: 0x602a, label: null, state: "ready", capabilities: [] };
}

const scope = device("3-3.4.2");
const analyzer = device("3-3.4.1.1");

describe("chooseSelection", () => {
  it("keeps the selected device while it is on the bus", () => {
    expect(chooseSelection([analyzer, scope], scope.id, scope)).toEqual({ id: scope.id, info: scope, departed: false });
  });

  it("keeps a device that left the bus selected instead of jumping to another one", () => {
    // The scope power-cycled under Live: the picker went to the analyzer and
    // stayed there when the scope came back.
    expect(chooseSelection([analyzer], scope.id, scope)).toEqual({ id: scope.id, info: scope, departed: true });
  });

  it("selects the device again, as on the bus, when it comes back", () => {
    const back = { ...scope, state: "ready" as const };
    expect(chooseSelection([analyzer, back], scope.id, scope)).toEqual({ id: scope.id, info: back, departed: false });
  });

  it("falls back to the first device only without a prior selection", () => {
    expect(chooseSelection([analyzer, scope], null, null)).toEqual({ id: analyzer.id, info: analyzer, departed: false });
    expect(chooseSelection([], null, null)).toEqual({ id: null, info: null, departed: false });
  });
});
