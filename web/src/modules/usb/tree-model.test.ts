import { describe, expect, it } from "vitest";
import type { DeviceInfo, UsbTreeResult } from "../../proto/types";
import { UsbTreeModel, collisionNote, flattenTree, formatId, isEmptyPort, overlayDeviceStates } from "./tree-model";
import { benchTree } from "./fixtures";

describe("formatId", () => {
  it("matches the DeviceId bus-path format used by DeviceInfo.id and event `device` fields", () => {
    expect(formatId(3, [3, 4, 2])).toBe("3-3.4.2");
    expect(formatId(3, [3])).toBe("3-3");
  });
});

describe("isEmptyPort", () => {
  it("is true only when a port has no device attached", () => {
    expect(isEmptyPort({ device: null })).toBe(true);
    expect(isEmptyPort({ device: undefined })).toBe(true);
    expect(isEmptyPort({ device: { bus: 3, port_path: [3, 3], vid: 1, pid: 2, is_hub: false, is_instrument: true, ports: [] } })).toBe(
      false,
    );
  });
});

describe("flattenTree — bench topology", () => {
  const rows = flattenTree(benchTree());
  const byKey = new Map(rows.map((r) => [r.key, r]));

  it("produces one row per node and per port, hubs, leaves and empty ports alike, depth-first", () => {
    expect(rows.map((r) => r.key)).toEqual([
      "3-3",
      "3-3.1",
      "3-3.2",
      "3-3.3",
      "3-3.4",
      "3-3.4.1",
      "3-3.4.2",
      "3-3.4.3",
      "3-3.4.4",
    ]);
  });

  it("shows empty ports as rows, not just populated ones", () => {
    const port1 = byKey.get("3-3.1");
    expect(port1?.empty).toBe(true);
    expect(port1?.portNumber).toBe(1);
    expect(port1?.vid).toBeNull();
    expect(port1?.pid).toBeNull();
    expect(port1?.hasToggle).toBe(true);
  });

  it("reports port number as the port's own number, independent of what's attached", () => {
    expect(byKey.get("3-3.4.2")?.portNumber).toBe(2);
    expect(byKey.get("3-3.3")?.portNumber).toBe(3);
  });

  it("reports a hub's port count from its own ports array", () => {
    expect(byKey.get("3-3")?.portCount).toBe(4);
    expect(byKey.get("3-3.4")?.portCount).toBe(4);
    expect(byKey.get("3-3.4.2")?.portCount).toBeNull(); // leaf, not a hub
  });

  it("carries ppps only on hub nodes", () => {
    expect(byKey.get("3-3")?.ppps).toBe(true);
    expect(byKey.get("3-3.4")?.ppps).toBe(true);
    expect(byKey.get("3-3.3")?.ppps).toBeNull();
  });

  it("marks hub 3-3 port 4 protected and non-switchable with its reason, per SPEC §4.4", () => {
    const port4 = byKey.get("3-3.4");
    expect(port4?.switchable).toBe(false);
    expect(port4?.protected).toBe(true);
    expect(port4?.protection).toBe("feeds hub 3-3.4: switching it cuts every device behind it");
    expect(port4?.nonSwitchableReason).toBe("feeds hub 3-3.4: switching it cuts every device behind it");
    expect(port4?.hasToggle).toBe(true); // still a real port row, just disabled
    expect(port4?.powerSwitchable).toBe(true); // the hub *can* switch it — it's protection, not a hardware limit
    expect(port4?.userChoice).toBeNull(); // automatic, not a user lock
  });

  it("marks unprotected leaf and empty ports switchable", () => {
    expect(byKey.get("3-3.3")?.switchable).toBe(true);
    expect(byKey.get("3-3.3")?.protected).toBe(false);
    expect(byKey.get("3-3.1")?.switchable).toBe(true);
    expect(byKey.get("3-3.4.2")?.switchable).toBe(true);
  });

  it("gives the depth-0 root row no power-switchable/protection fields to act on", () => {
    const root = byKey.get("3-3");
    expect(root?.powerSwitchable).toBe(false);
    expect(root?.protected).toBe(false);
    expect(root?.protection).toBeNull();
    expect(root?.userChoice).toBeNull();
  });

  it("computes each port row's parent hub id for usb.power targeting", () => {
    expect(byKey.get("3-3.3")?.parentHub).toBe("3-3");
    expect(byKey.get("3-3.4.2")?.parentHub).toBe("3-3.4");
  });

  it("gives the depth-0 root row no toggle — the real root-hub port isn't modeled on the wire", () => {
    const root = byKey.get("3-3");
    expect(root?.hasToggle).toBe(false);
    expect(root?.portNumber).toBeNull();
    expect(root?.parentHub).toBeNull();
  });

  it("reports depth for indentation", () => {
    expect(byKey.get("3-3")?.depth).toBe(0);
    expect(byKey.get("3-3.4")?.depth).toBe(1);
    expect(byKey.get("3-3.4.2")?.depth).toBe(2);
  });

  it("reports power state and instrument flag", () => {
    expect(byKey.get("3-3.3")?.power).toBe(true);
    expect(byKey.get("3-3.3")?.isInstrument).toBe(true);
    expect(byKey.get("3-3.1")?.power).toBe(false);
    expect(byKey.get("3-3.1")?.isInstrument).toBe(false);
  });
});

describe("collisionNote — SPEC §5.1", () => {
  it("flags the shared 0925:3881 VID:PID so the two devices stay distinguishable", () => {
    expect(collisionNote(0x0925, 0x3881)).toMatch(/logic analyzer/i);
    expect(collisionNote(0x0925, 0x3881)).toMatch(/logic/i);
  });

  it("is null for VID:PID pairs with no known collision, and for empty ports", () => {
    expect(collisionNote(0x04b4, 0x602a)).toBeNull();
    expect(collisionNote(0x2109, 0x2822)).toBeNull();
    expect(collisionNote(null, null)).toBeNull();
  });
});

describe("overlayDeviceStates", () => {
  it("overlays lifecycle state from the flat device registry by bus-path key", () => {
    const rows = flattenTree(benchTree());
    const devices: DeviceInfo[] = [{ id: "3-3.4.2", vid: 0x04b4, pid: 0x602a, state: "flashing", capabilities: [] }];
    const overlaid = overlayDeviceStates(rows, devices);
    const scope = overlaid.find((r) => r.key === "3-3.4.2");
    expect(scope?.deviceState).toBe("flashing");
  });

  it("leaves deviceState null for rows absent from the registry (e.g. empty ports, hubs)", () => {
    const overlaid = overlayDeviceStates(flattenTree(benchTree()), []);
    expect(overlaid.every((r) => r.deviceState === null)).toBe(true);
  });
});

describe("UsbTreeModel — flashing -> ready as one continuous transition (SPEC §4.5)", () => {
  it("updates the same node in place when VID:PID changes at the same bus path, never removing it", () => {
    const model = new UsbTreeModel();

    const flashing = model.apply(
      benchTree({ vid: 0x04b4, pid: 0x602a, product: "FX2 oscilloscope (unprogrammed)" }),
      [{ id: "3-3.4.2", vid: 0x04b4, pid: 0x602a, state: "flashing", capabilities: [] }],
    );
    expect(flashing.rows.filter((r) => r.key === "3-3.4.2")).toHaveLength(1);
    expect(model.get("3-3.4.2")?.pid).toBe(0x602a);
    expect(model.get("3-3.4.2")?.vid).toBe(0x04b4);
    expect(model.get("3-3.4.2")?.deviceState).toBe("flashing");

    // Re-enumerated: same bus path, new PID, now ready.
    const ready = model.apply(
      benchTree({ vid: 0x04b5, pid: 0x602a, product: "FX2 oscilloscope" }),
      [{ id: "3-3.4.2", vid: 0x04b5, pid: 0x602a, state: "ready", capabilities: [] }],
    );

    // Never reported as removed, and there is still exactly one row for it.
    expect(ready.removedKeys).not.toContain("3-3.4.2");
    expect(ready.rows.filter((r) => r.key === "3-3.4.2")).toHaveLength(1);
    expect(model.size).toBe(flashing.rows.length);

    const updated = model.get("3-3.4.2");
    expect(updated?.vid).toBe(0x04b5);
    expect(updated?.deviceState).toBe("ready");
    expect(ready.changedKeys).toContain("3-3.4.2");
  });

  it("reports a path as removed only when the server's tree genuinely drops the port itself", () => {
    const model = new UsbTreeModel();
    model.apply(benchTree());

    const withoutInstrumentHub: UsbTreeResult = benchTree();
    const upstream = withoutInstrumentHub.roots[0];
    if (!upstream) throw new Error("fixture missing root");
    // Drop hub 3-3.4 (and everything under it) entirely, not just power it off.
    upstream.ports = upstream.ports.filter((p) => p.number !== 4);

    const after = model.apply(withoutInstrumentHub);
    expect(after.removedKeys).toEqual(expect.arrayContaining(["3-3.4", "3-3.4.1", "3-3.4.2", "3-3.4.3", "3-3.4.4"]));
    expect(model.get("3-3.4.2")).toBeUndefined();
  });

  it("keeps the port row and its (enabled) toggle after the port is powered off and its device disappears", () => {
    const model = new UsbTreeModel();

    const before = model.apply(benchTree());
    const logicBefore = before.rows.find((r) => r.key === "3-3.3");
    if (!logicBefore) throw new Error("fixture missing 3-3.3");
    expect(logicBefore.empty).toBe(false);
    expect(logicBefore.switchable).toBe(true);

    // Port 3 is switched off: the device drops off the bus, but the UsbPort
    // itself — and therefore the row — must still be there, still switchable,
    // now empty and powered off. This is the only way the user can ever turn
    // it back on (M1's exit test: the LA must be able to reappear).
    const poweredOff: UsbTreeResult = benchTree();
    const upstream = poweredOff.roots[0];
    if (!upstream) throw new Error("fixture missing root");
    const port3 = upstream.ports.find((p) => p.number === 3);
    if (!port3) throw new Error("fixture missing port 3");
    port3.device = null;
    port3.power = false;

    const after = model.apply(poweredOff);

    expect(after.removedKeys).not.toContain("3-3.3");
    const logicAfter = after.rows.find((r) => r.key === "3-3.3");
    expect(logicAfter).toBeDefined();
    expect(logicAfter?.empty).toBe(true);
    expect(logicAfter?.power).toBe(false);
    expect(logicAfter?.switchable).toBe(true); // the toggle is still enabled — the user can power it back on
    expect(logicAfter?.hasToggle).toBe(true);
  });

  it("does not report unchanged rows as changed on a re-apply with identical data", () => {
    const model = new UsbTreeModel();
    model.apply(benchTree());
    const second = model.apply(benchTree());
    expect(second.changedKeys).toEqual([]);
    expect(second.removedKeys).toEqual([]);
    expect(second.addedKeys).toEqual([]);
  });
});
