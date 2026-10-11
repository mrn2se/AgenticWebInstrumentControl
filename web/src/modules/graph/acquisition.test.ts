import { describe, expect, it } from "vitest";
import {
  applyAcqStateEvent,
  applyStopped,
  applyStreamStarted,
  canStart,
  canStop,
  initialAcquisitionState,
} from "./acquisition";

describe("acquisition state (server-authoritative, SPEC §11)", () => {
  it("starts idle, with start allowed and stop not", () => {
    const state = initialAcquisitionState();
    expect(state.state).toBe("idle");
    expect(canStart(state)).toBe(true);
    expect(canStop(state)).toBe(false);
  });

  it("does not optimistically become 'running' just because acq.start resolved", () => {
    const started = applyStreamStarted(initialAcquisitionState(), { stream: 7, capture: "capture://abc" });
    expect(started.streamId).toBe(7);
    expect(started.captureHandle).toBe("capture://abc");
    expect(started.state).toBe("idle"); // still waiting on the server's acq.state push
  });

  it("adopts state only from an acq.state event matching the tracked stream id", () => {
    const started = applyStreamStarted(initialAcquisitionState(), { stream: 7, capture: "capture://abc" });
    const running = applyAcqStateEvent(started, { stream: 7, state: "running" });
    expect(running.state).toBe("running");
    expect(canStop(running)).toBe(true);
    expect(canStart(running)).toBe(false);
  });

  it("ignores an acq.state event for a different stream", () => {
    const started = applyStreamStarted(initialAcquisitionState(), { stream: 7, capture: "capture://abc" });
    const unaffected = applyAcqStateEvent(started, { stream: 99, state: "running" });
    expect(unaffected).toEqual(started);
  });

  it("carries the server's error message through unchanged", () => {
    const started = applyStreamStarted(initialAcquisitionState(), { stream: 7, capture: "capture://abc" });
    const errored = applyAcqStateEvent(started, { stream: 7, state: "error", msg: "device disconnected" });
    expect(errored.state).toBe("error");
    expect(errored.message).toBe("device disconnected");
    expect(canStart(errored)).toBe(true);
  });

  it("resets stream tracking on stop", () => {
    const started = applyStreamStarted(initialAcquisitionState(), { stream: 7, capture: "capture://abc" });
    const stopped = applyStopped(started);
    expect(stopped.streamId).toBeNull();
    expect(stopped.state).toBe("idle");
  });
});
