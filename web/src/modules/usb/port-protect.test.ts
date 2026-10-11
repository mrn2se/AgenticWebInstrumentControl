import { describe, expect, it } from "vitest";
import { PortProtectTracker } from "./port-protect";
import { portKey } from "./port-power";

describe("PortProtectTracker", () => {
  it("starts idle for a key it has never seen", () => {
    const tracker = new PortProtectTracker();
    expect(tracker.get(portKey("3-3", 4))).toEqual({ phase: "idle" });
  });

  it("goes pending on begin(), never carrying a protected/user_choice value to flip to", () => {
    const tracker = new PortProtectTracker();
    const key = portKey("3-3", 4);
    tracker.begin(key);
    const state = tracker.get(key);
    expect(state.phase).toBe("pending");
    expect(state).not.toHaveProperty("protected");
    expect(state).not.toHaveProperty("userChoice");
  });

  it("returns to idle only once resolve() is called (i.e. the server responded)", () => {
    const tracker = new PortProtectTracker();
    const key = portKey("3-3", 4);
    tracker.begin(key);
    tracker.resolve(key);
    expect(tracker.get(key)).toEqual({ phase: "idle" });
  });

  it("surfaces a failed usb.protect the same way a failed usb.power is surfaced", () => {
    const tracker = new PortProtectTracker();
    const key = portKey("3-3", 4);
    tracker.begin(key);
    tracker.fail(key, { code: "state_write_failed", msg: "could not persist usb-ports.json" });
    const state = tracker.get(key);
    expect(state.phase).toBe("error");
    expect(state.error).toEqual({ code: "state_write_failed", msg: "could not persist usb-ports.json" });
  });

  it("clear() returns an error state to idle", () => {
    const tracker = new PortProtectTracker();
    const key = portKey("3-3", 4);
    tracker.fail(key, { code: "state_write_failed", msg: "no" });
    tracker.clear(key);
    expect(tracker.get(key)).toEqual({ phase: "idle" });
  });

  it("tracks independent keys independently, and independently of a power tracker on the same port", () => {
    const tracker = new PortProtectTracker();
    tracker.begin(portKey("3-3", 4));
    expect(tracker.get(portKey("3-3", 3))).toEqual({ phase: "idle" });
  });
});
