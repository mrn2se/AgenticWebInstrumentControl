import { describe, expect, it } from "vitest";
import {
  ANALOG_MEASUREMENT_KINDS,
  DIGITAL_MEASUREMENT_KINDS,
  measurementRows,
  requestMeasurement,
  type MeasurementRequest,
  type SendCaptureMeasure,
} from "./measurements";
import type { CaptureMeasureResult } from "../../proto/types";

const request: MeasurementRequest = {
  capture: "capture://abc",
  channel: 0,
  label: "D0",
  kinds: ["frequency", "vpp"],
};

describe("requestMeasurement (SPEC §5.4: server-side only, real capture.measure op)", () => {
  it("sends exactly the requested capture, channel and kinds", async () => {
    let sent: unknown;
    const send: SendCaptureMeasure = async (params) => {
      sent = params;
      return { capture: request.capture, channel: request.channel, results: [] };
    };
    await requestMeasurement(send, request);
    expect(sent).toEqual({
      capture: "capture://abc",
      channel: 0,
      measurements: ["frequency", "vpp"],
    });
  });

  it("maps an ok wire result into an ok entry with value and unit", async () => {
    const result: CaptureMeasureResult = {
      capture: request.capture,
      channel: 0,
      results: [{ kind: "frequency", value: 1000.2, unit: "Hz" }],
    };
    const send: SendCaptureMeasure = async () => result;
    const status = await requestMeasurement(send, request);
    expect(status).toEqual({
      status: "ok",
      request,
      entries: [{ kind: "frequency", outcome: { status: "ok", value: 1000.2, unit: "Hz" } }],
    });
  });

  it("maps an unavailable wire result honestly, never as a number or a blank", async () => {
    const result: CaptureMeasureResult = {
      capture: request.capture,
      channel: 0,
      results: [{ kind: "duty", unavailable: "capture is shorter than one period" }],
    };
    const send: SendCaptureMeasure = async () => result;
    const status = await requestMeasurement(send, request);
    expect(status).toEqual({
      status: "ok",
      request,
      entries: [
        { kind: "duty", outcome: { status: "unavailable", reason: "capture is shorter than one period" } },
      ],
    });
  });

  it("reports a transport/RPC failure as a typed error, not a thrown exception", async () => {
    const send: SendCaptureMeasure = async () => {
      throw new Error("request timed out");
    };
    const status = await requestMeasurement(send, request);
    expect(status).toEqual({ status: "error", request, message: "request timed out" });
  });

  it("lists the SPEC §5.4 analog and digital measurement kinds", () => {
    expect(ANALOG_MEASUREMENT_KINDS).toEqual(
      expect.arrayContaining(["vpp", "vmin", "vmax", "vavg", "vrms", "frequency", "period", "duty"]),
    );
    expect(DIGITAL_MEASUREMENT_KINDS).toEqual(
      expect.arrayContaining(["frequency", "period", "duty", "edge_count"]),
    );
    // Positive and negative pulse width are distinct digital measurements.
    expect(DIGITAL_MEASUREMENT_KINDS).toContain("pulse_width_high");
    expect(DIGITAL_MEASUREMENT_KINDS).toContain("pulse_width_low");
  });
});

describe("measurementRows (panel rendering: values vs. unavailability, never a blank)", () => {
  it("renders a pending request as one row per requested kind", () => {
    const rows = measurementRows({ status: "pending", request });
    expect(rows).toEqual([
      { label: "D0", kind: "frequency", text: "measuring…" },
      { label: "D0", kind: "vpp", text: "measuring…" },
    ]);
  });

  it("renders an ok value with its unit", () => {
    const rows = measurementRows({
      status: "ok",
      request,
      entries: [{ kind: "frequency", outcome: { status: "ok", value: 1000.2, unit: "Hz" } }],
    });
    expect(rows).toEqual([{ label: "D0", kind: "frequency", text: "1000.20 Hz" }]);
  });

  it("renders an integer value without spurious decimals", () => {
    const rows = measurementRows({
      status: "ok",
      request,
      entries: [{ kind: "edge_count", outcome: { status: "ok", value: 39, unit: "count" } }],
    });
    expect(rows).toEqual([{ label: "D0", kind: "edge_count", text: "39 count" }]);
  });

  it("renders unavailability as an explanatory reason, never a blank or a number", () => {
    const rows = measurementRows({
      status: "ok",
      request,
      entries: [
        {
          kind: "duty",
          outcome: { status: "unavailable", reason: "capture is shorter than one period" },
        },
      ],
    });
    expect(rows).toEqual([
      { label: "D0", kind: "duty", text: "unavailable — capture is shorter than one period" },
    ]);
  });

  it("renders a transport error for every requested kind", () => {
    const rows = measurementRows({ status: "error", request, message: "socket closed" });
    expect(rows).toEqual([
      { label: "D0", kind: "frequency", text: "error: socket closed" },
      { label: "D0", kind: "vpp", text: "error: socket closed" },
    ]);
  });
});
