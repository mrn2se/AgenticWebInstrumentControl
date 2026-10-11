import { describe, expect, it } from "vitest";
import type { RecordingSummary } from "../../proto/types";
import { activityLine, deviceActivity, stopTarget, type ActivityInput } from "./device-activity";
import { applyAcqStateEvent, applyStopped, applyStreamStarted, initialAcquisitionState } from "./acquisition";

function recording(state: RecordingSummary["state"], msg: string | null = null): RecordingSummary {
  return {
    rec: 7,
    device: "3-3.4.1.1",
    kind: "digital",
    rate_hz: 1_000_000,
    channels: [],
    first_sample: 0,
    head_sample: 1_000,
    bytes: 128,
    ratio: 2,
    state,
    ...(msg === null ? {} : { msg }),
  } as RecordingSummary;
}

function input(over: Partial<ActivityInput>): ActivityInput {
  return {
    mode: "single",
    acq: initialAcquisitionState(),
    phase: "none",
    recording: null,
    ...over,
  };
}

// B-67: for the 50–300 ms between the click and the server's reply the toolbar
// said "idle" with Start enabled, so a second click sent a second start — the
// client half of the recorder race (B-59) — and a Stop in that window did
// nothing while the run carried on.
describe("a start that has not been answered yet", () => {
  it("is not idle, and cannot be started again", () => {
    const activity = deviceActivity(input({ phase: "starting" }));
    expect(activity.kind).toBe("starting");
    expect(activity.canStart).toBe(false);
    expect(activityLine(activity)).toContain("starting");
  });

  it("can be stopped, so Stop in that window is not a no-op", () => {
    expect(deviceActivity(input({ phase: "starting" })).canStop).toBe(true);
  });

  it("outranks whatever the device last reported", () => {
    // A stale "running" from the previous stream must not re-enable Start.
    const running = applyAcqStateEvent(applyStreamStarted(initialAcquisitionState(), { stream: 1, capture: "capture://x" }), {
      stream: 1,
      state: "running",
    });
    const activity = deviceActivity(input({ acq: running, phase: "starting" }));
    expect(activity.kind).toBe("starting");
  });

  it("disables both buttons while a stop is in flight", () => {
    const activity = deviceActivity(input({ phase: "stopping" }));
    expect(activity.canStart).toBe(false);
    expect(activity.canStop).toBe(false);
  });
});

// B-66: Single and Live each read their own half of the truth. A recording
// holding the device left Single offering a Start the server refused with
// `device_busy`, and a recording that had died showed as a bare "error" in
// Live and stayed "recording" in Single.
describe("a recording holding the device", () => {
  it("is reported in Single mode too, with Stop offered rather than Start", () => {
    const activity = deviceActivity(input({ mode: "single", recording: recording("recording") }));
    expect(activity.kind).toBe("recording");
    expect(activity.canStart).toBe(false);
    expect(activity.canStop).toBe(true);
    expect(activity.detail).toContain("recording 7");
  });

  it("reads the same in Live mode", () => {
    const activity = deviceActivity(input({ mode: "live", recording: recording("recording") }));
    expect(activity.kind).toBe("recording");
    expect(activity.canStart).toBe(false);
    expect(activity.canStop).toBe(true);
  });

  it("shows the reason it died in either mode", () => {
    const why = "the device left the bus after 0 samples";
    for (const mode of ["single", "live"] as const) {
      const activity = deviceActivity(input({ mode, recording: recording("error", why) }));
      expect(activity.kind).toBe("error");
      expect(activity.detail).toBe(why);
      // The device is free again, so a new run is allowed.
      expect(activity.canStart).toBe(true);
      expect(activityLine(activity)).toBe(`error — ${why}`);
    }
  });

  // B-31: a reloaded tab finds a recording it did not start (a closed tab's,
  // or an agent's). Live mode's Start is how it attaches, so Start must stay
  // enabled for one it is not already showing, and Stop must reach it.
  it("can be attached to in Live mode when this tab is not showing it", () => {
    const activity = deviceActivity(input({ mode: "live", recording: recording("recording"), watching: false }));
    expect(activity.kind).toBe("recording");
    expect(activity.canStart).toBe(true);
    expect(activity.canStop).toBe(true);
    expect(activity.detail).toContain("recording 7");
  });

  it("is still not startable in Single mode when this tab is not showing it", () => {
    const activity = deviceActivity(input({ mode: "single", recording: recording("recording"), watching: false }));
    expect(activity.canStart).toBe(false);
    expect(activity.canStop).toBe(true);
  });

  it("does not hold the device once it is stopped", () => {
    const activity = deviceActivity(input({ mode: "live", recording: recording("stopped") }));
    expect(activity.canStart).toBe(true);
    expect(activity.canStop).toBe(false);
  });
});

// The other half of B-67, found by the start/stop run after the first fix: once
// the reply has named a stream the module is back to `state: "idle"` until the
// server's first `acq.state` event, which on a local server is where most of the
// window is. Start must stay disabled across that too.
describe("a start that has been answered but not reported on", () => {
  it("is not idle, and Start stays disabled until the device reports", () => {
    const started = applyStreamStarted(initialAcquisitionState(), { stream: 5, capture: "capture://z" });
    const activity = deviceActivity(input({ acq: started }));
    expect(activity.kind).toBe("starting");
    expect(activity.canStart).toBe(false);
    expect(activity.canStop).toBe(true);
    expect(activityLine(activity)).toContain("waiting for the device");
  });

  it("gives way to whatever the device then reports", () => {
    const started = applyStreamStarted(initialAcquisitionState(), { stream: 5, capture: "capture://z" });
    const running = applyAcqStateEvent(started, { stream: 5, state: "running" });
    expect(deviceActivity(input({ acq: running })).kind).toBe("running");
    const stopped = applyAcqStateEvent(started, { stream: 5, state: "stopped", msg: "complete" });
    const activity = deviceActivity(input({ acq: stopped }));
    expect(activity.kind).toBe("stopped");
    expect(activity.canStart).toBe(true);
  });
});

// The other direction of B-66, found by the start/stop run: a finite
// acquisition holds the device as much as a recording does, and Live read
// "idle" over a running capture with Start enabled - so a rec.start went out
// and came back `device_busy`.
describe("a single capture holding the device", () => {
  it("is reported in Live mode, with Stop offered rather than Start", () => {
    const running = applyAcqStateEvent(applyStreamStarted(initialAcquisitionState(), { stream: 2, capture: "capture://a" }), {
      stream: 2,
      state: "running",
    });
    const activity = deviceActivity(input({ mode: "live", acq: running }));
    expect(activity.kind).toBe("running");
    expect(activity.canStart).toBe(false);
    expect(activity.canStop).toBe(true);
    expect(activity.detail).toContain("single capture");
  });

  it("covers a stream that has not reported yet", () => {
    const started = applyStreamStarted(initialAcquisitionState(), { stream: 2, capture: "capture://a" });
    expect(deviceActivity(input({ mode: "live", acq: started })).canStart).toBe(false);
  });

  it("releases Live once the capture is done", () => {
    const stopped = applyStopped(
      applyAcqStateEvent(applyStreamStarted(initialAcquisitionState(), { stream: 2, capture: "capture://a" }), {
        stream: 2,
        state: "stopped",
      }),
    );
    const activity = deviceActivity(input({ mode: "live", acq: stopped }));
    expect(activity.canStart).toBe(true);
  });

  // A capture that completes on its own reports `stopped` and keeps its stream
  // id (Single mode still needs the capture handle); Live's Start stayed
  // disabled after it until the page was reloaded.
  it("releases Live once the capture completes on its own, or fails", () => {
    for (const state of ["stopped", "error"] as const) {
      const done = applyAcqStateEvent(applyStreamStarted(initialAcquisitionState(), { stream: 2, capture: "capture://a" }), {
        stream: 2,
        state,
      });
      const activity = deviceActivity(input({ mode: "live", acq: done }));
      expect(activity.canStart).toBe(true);
      expect(activity.canStop).toBe(false);
    }
  });
});

describe("Single mode with no recording", () => {
  it("follows the acquisition state, as it always did", () => {
    const idle = deviceActivity(input({}));
    expect(idle.kind).toBe("idle");
    expect(idle.canStart).toBe(true);
    expect(idle.canStop).toBe(false);

    const armed = applyAcqStateEvent(applyStreamStarted(initialAcquisitionState(), { stream: 3, capture: "capture://y" }), {
      stream: 3,
      state: "armed",
      msg: "waiting for the trigger",
    });
    const activity = deviceActivity(input({ acq: armed }));
    expect(activity.kind).toBe("armed");
    expect(activity.canStop).toBe(true);
    expect(activityLine(activity)).toBe("armed — waiting for the trigger");
  });
});

// Both directions of "Stop reaches what holds the device". Each was an enabled
// button that did nothing, found by the start/stop run rather than by reading.
describe("stopTarget", () => {
  it("stops the recording when one holds the device", () => {
    expect(stopTarget(true, false)).toBe("recording");
    // Even with a stream of this client's own in the picture.
    expect(stopTarget(true, true)).toBe("recording");
  });

  it("stops the capture when a stream holds the device", () => {
    expect(stopTarget(false, true)).toBe("stream");
  });

  it("falls back to the mode when nothing holds the device", () => {
    expect(stopTarget(false, false)).toBe("mode-default");
  });

  // It takes facts, not a `DeviceActivity`: a caller that had already entered
  // its own "stopping" phase handed that in, the router read "stopping"
  // instead of the recording, and the Stop went nowhere with a recording still
  // running - the third time this button did nothing.
  it("is not shadowed by a pending phase the caller is displaying", () => {
    const stopping = deviceActivity(input({ phase: "stopping", recording: recording("recording") }));
    expect(stopping.kind).toBe("stopping");
    expect(stopTarget(true, false)).toBe("recording");
  });
});
