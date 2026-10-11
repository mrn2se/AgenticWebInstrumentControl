import { describe, expect, it } from "vitest";
import type { DeviceInfo, Event as ServerEvent } from "../proto/types";
import { applyServerEvent, createInitialState } from "./state";

const DEVICE: DeviceInfo = {
  id: "3-3.4.2",
  vid: 0x04b5,
  pid: 0x602a,
  label: null,
  state: "ready",
  capabilities: [],
};

describe("applyServerEvent", () => {
  it("leaves the instrument registry untouched on usb.changed (the bus is not the registry)", () => {
    const before = createInitialState();
    const event: ServerEvent = { event: "usb.changed", v: 1, device: "3-3.3", change: "removed" };
    expect(applyServerEvent(before, event)).toBe(before);
  });

  it("adds a device on device.added", () => {
    const event: ServerEvent = { event: "device.added", v: 1, device: DEVICE };
    const next = applyServerEvent(createInitialState(), event);
    expect(next.devices).toEqual([DEVICE]);
  });

  it("removes a device on device.removed", () => {
    const withDevice = applyServerEvent(createInitialState(), {
      event: "device.added",
      v: 1,
      device: DEVICE,
    });
    const next = applyServerEvent(withDevice, { event: "device.removed", v: 1, device: DEVICE.id });
    expect(next.devices).toEqual([]);
  });

  it("updates only the state field on device.state, mirroring the server's word for it", () => {
    const withDevice = applyServerEvent(createInitialState(), {
      event: "device.added",
      v: 1,
      device: DEVICE,
    });
    const next = applyServerEvent(withDevice, {
      event: "device.state",
      v: 1,
      device: DEVICE.id,
      state: "flashing",
    });
    expect(next.devices[0]?.state).toBe("flashing");
    expect(next.devices[0]?.id).toBe(DEVICE.id);
  });

  it("surfaces an acq.state error event as a visible error, not a silent drop", () => {
    const next = applyServerEvent(createInitialState(), {
      event: "acq.state",
      v: 1,
      stream: 7,
      state: "error",
      msg: "device unplugged mid-capture",
    });
    expect(next.errors).toHaveLength(1);
    expect(next.errors[0]?.msg).toBe("device unplugged mid-capture");
  });

  it("does not touch devices for a non-error acq.state event", () => {
    const initial = createInitialState();
    const next = applyServerEvent(initial, { event: "acq.state", v: 1, stream: 7, state: "armed" });
    expect(next).toEqual(initial);
  });
});
