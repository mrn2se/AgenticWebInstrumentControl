import { describe, expect, it } from "vitest";
import {
  ANALOG_MEASUREMENT_KINDS,
  DIGITAL_MEASUREMENT_KINDS,
  addMeasurementStatus,
  captureHeader,
  formatMeasurementValue,
  measurementBlocks,
  measurementRows,
  requestMeasurement,
  snapshotSampleCount,
  type MeasurementStatus,
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
    expect(rows).toEqual([{ label: "D0", kind: "frequency", text: "1.000 kHz" }]);
  });

  it("renders an integer value without spurious decimals", () => {
    const rows = measurementRows({
      status: "ok",
      request,
      entries: [{ kind: "edge_count", outcome: { status: "ok", value: 39, unit: "count" } }],
    });
    expect(rows).toEqual([{ label: "D0", kind: "edge_count", text: "39" }]);
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

describe("formatMeasurementValue (SI prefixes, no false precision)", () => {
  it("formats times with SI prefixes", () => {
    expect(formatMeasurementValue("period", 0.001, "s")).toBe("1.000 ms");
    expect(formatMeasurementValue("rise_time", 8.18705e-7, "s")).toBe("819 ns");
    expect(formatMeasurementValue("fall_time", 1.64226e-7, "s")).toBe("164 ns");
  });

  it("rounds volts to the channel's resolution", () => {
    const step = 10 / 255; // 8-bit converter on a ±5 V range
    expect(formatMeasurementValue("vpp", 2.0, "V", step)).toBe("2.00 V");
    expect(formatMeasurementValue("vmin", 0.254902, "V", step)).toBe("0.25 V");
  });

  it("falls back to 3 significant digits for volts without a resolution", () => {
    expect(formatMeasurementValue("vmin", 0.254902, "V")).toBe("255 mV");
  });

  it("formats percentages to a tenth", () => {
    expect(formatMeasurementValue("duty", 49.98765, "%")).toBe("50.0 %");
  });
});

describe("measurement blocks per capture", () => {
  const ok = (capture: string, channel: number): MeasurementStatus => ({
    status: "ok",
    request: { capture, channel, label: `CH${channel + 1}`, kinds: ["vpp"] },
    entries: [],
  });

  it("groups by capture, newest first", () => {
    let statuses: MeasurementStatus[] = [];
    statuses = addMeasurementStatus(statuses, ok("capture://a", 0));
    statuses = addMeasurementStatus(statuses, ok("capture://b", 0));
    statuses = addMeasurementStatus(statuses, ok("capture://b", 1));
    const blocks = measurementBlocks(statuses);
    expect(blocks.map((b) => b.capture)).toEqual(["capture://b", "capture://a"]);
    expect(blocks[0]!.statuses.map((s) => s.request.channel)).toEqual([1, 0]);
  });

  it("orders blocks by capture time, not by the last Measure click", () => {
    let statuses: MeasurementStatus[] = [];
    statuses = addMeasurementStatus(statuses, ok("capture://a", 0));
    statuses = addMeasurementStatus(statuses, ok("capture://b", 0));
    statuses = addMeasurementStatus(statuses, ok("capture://a", 1)); // re-measure the older capture
    const times: Record<string, number> = { "capture://a": 1000, "capture://b": 2000 };
    expect(measurementBlocks(statuses, (c) => times[c]).map((b) => b.capture)).toEqual(["capture://b", "capture://a"]);
  });

  it("replaces an earlier measurement of the same channel of the same capture", () => {
    let statuses: MeasurementStatus[] = [];
    statuses = addMeasurementStatus(statuses, ok("capture://a", 0));
    statuses = addMeasurementStatus(statuses, ok("capture://a", 0));
    expect(statuses).toHaveLength(1);
  });

  it("heads a block with time, rate and sample count", () => {
    const takenAtMs = new Date(2026, 0, 1, 14, 3, 22).getTime();
    expect(captureHeader({ capture: "capture://a", takenAtMs, rateHz: 1e6, samples: 10000, source: "single" })).toBe(
      "Capture 14:03:22 · 1 MS/s · 10\u202f000 samples",
    );
  });
});

describe("snapshotSampleCount", () => {
  it("counts the samples between the rounded ends", () => {
    expect(snapshotSampleCount({ t0: 0.001, t1: 0.003 }, 1e6)).toBe(2000);
  });

  it("keeps the range inside what the recording still holds", () => {
    expect(snapshotSampleCount({ t0: 0.001, t1: 0.003 }, 1e6, { first_sample: 0, head_sample: 2500 })).toBe(1500);
    expect(snapshotSampleCount({ t0: 0.001, t1: 0.003 }, 1e6, { first_sample: 1800, head_sample: 5000 })).toBe(1200);
  });

  it("takes negative times as zero", () => {
    expect(snapshotSampleCount({ t0: -0.001, t1: 0.001 }, 1e6)).toBe(1000);
  });

  it("is zero without a rate", () => {
    expect(snapshotSampleCount({ t0: 0, t1: 1 }, 0)).toBe(0);
  });
});
