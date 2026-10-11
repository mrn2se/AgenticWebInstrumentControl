import { describe, expect, it } from "vitest";
import { PortPowerTracker, portKey } from "./port-power";
import { UsbTreeModel } from "./tree-model";
import { benchTree } from "./fixtures";

describe("PortPowerTracker", () => {
  it("starts idle for a key it has never seen", () => {
    const tracker = new PortPowerTracker();
    expect(tracker.get(portKey("3-3", 3))).toEqual({ phase: "idle" });
  });

  it("goes pending on begin(), and pending never carries an on/off value to flip to", () => {
    const tracker = new PortPowerTracker();
    const key = portKey("3-3", 3);
    tracker.begin(key);
    const state = tracker.get(key);
    expect(state.phase).toBe("pending");
    expect(state).not.toHaveProperty("on");
  });

  it("returns to idle only once resolve() is called (i.e. the server responded)", () => {
    const tracker = new PortPowerTracker();
    const key = portKey("3-3", 3);
    tracker.begin(key);
    expect(tracker.get(key).phase).toBe("pending");
    tracker.resolve(key);
    expect(tracker.get(key)).toEqual({ phase: "idle" });
  });

  it("surfaces a guardrail refusal from the server as an error state carrying the code and message", () => {
    const tracker = new PortPowerTracker();
    const key = portKey("3-3", 4);
    tracker.begin(key);
    tracker.fail(key, { code: "port_protected", msg: "port 4 feeds the instrument hub" });
    const state = tracker.get(key);
    expect(state.phase).toBe("error");
    expect(state.error).toEqual({ code: "port_protected", msg: "port 4 feeds the instrument hub" });
  });

  it("clear() returns an error state to idle, e.g. before a retry", () => {
    const tracker = new PortPowerTracker();
    const key = portKey("3-3", 4);
    tracker.fail(key, { code: "port_protected", msg: "no" });
    tracker.clear(key);
    expect(tracker.get(key)).toEqual({ phase: "idle" });
  });

  it("tracks independent keys independently", () => {
    const tracker = new PortPowerTracker();
    tracker.begin(portKey("3-3", 3));
    expect(tracker.get(portKey("3-3", 4))).toEqual({ phase: "idle" });
  });
});

describe("toggle does not change reported power until the server confirms (SPEC §11)", () => {
  it("leaves PortRow.power untouched while a request is pending; only a fresh tree apply() changes it", () => {
    const model = new UsbTreeModel();
    const tracker = new PortPowerTracker();

    const before = model.apply(benchTree());
    const row = before.rows.find((r) => r.key === "3-3.3");
    if (!row) throw new Error("fixture missing 3-3.3");
    expect(row.power).toBe(true);

    // User clicks "off". A request goes out; nothing about the tree model
    // changes yet, only the tracker's pending flag — there is no code path
    // here that could set row.power to false optimistically.
    if (row.parentHub === null || row.portNumber === null) throw new Error("fixture row 3-3.3 unexpectedly has no toggle");
    const key = portKey(row.parentHub, row.portNumber);
    tracker.begin(key);
    expect(model.get("3-3.3")?.power).toBe(true);
    expect(tracker.get(key).phase).toBe("pending");

    // Server confirms via a fresh usb.tree response reflecting the new state.
    const confirmed = benchTree();
    const upstream = confirmed.roots[0];
    if (!upstream) throw new Error("fixture missing root");
    const port3 = upstream.ports.find((p) => p.number === 3);
    if (!port3) throw new Error("fixture missing port 3");
    port3.power = false;

    const after = model.apply(confirmed);
    tracker.resolve(key);

    expect(after.rows.find((r) => r.key === "3-3.3")?.power).toBe(false);
    expect(tracker.get(key).phase).toBe("idle");
  });
});
