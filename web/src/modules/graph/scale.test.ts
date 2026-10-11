import { describe, expect, it } from "vitest";
import type { ChannelCalibration } from "../../proto/types";
import { codeToUnits, identityCalibration } from "./scale";

// What a driver reports for an 8-bit channel on the ±5 V range: 0..255 spans
// -5..+5 V (the fake backend's exact conversion).
const pm5V: ChannelCalibration = { channel: 0, scale: 10 / 255, offset: -5, unit: "V", range_v: 5 };

describe("codeToUnits", () => {
  it("applies the driver's raw * scale + offset", () => {
    expect(codeToUnits(0, pm5V)).toBeCloseTo(-5);
    expect(codeToUnits(255, pm5V)).toBeCloseTo(5);
  });

  it("reads the range as ± full scale, not peak-to-peak", () => {
    // The protocol's ranges_v are ± full scale ("±5 V"). An earlier
    // client-side formula halved them, drawing this code at +2.5 V.
    expect(codeToUnits(255, pm5V)).not.toBeCloseTo(2.5, 1);
    expect(codeToUnits(255, pm5V)).toBeCloseTo(pm5V.range_v ?? Number.NaN);
  });

  it("carries calibration offsets only the driver knows", () => {
    // A per-range offset trim, as the FX2 oscilloscope stores on the device.
    const trimmed: ChannelCalibration = { ...pm5V, offset: -5 + 0.12 };
    expect(codeToUnits(0, trimmed)).toBeCloseTo(-4.88);
  });

  it("handles signed i16 codes through the calibration alone", () => {
    const i16: ChannelCalibration = { channel: 1, scale: 5 / 32768, offset: 0, unit: "V", range_v: 5 };
    expect(codeToUnits(-32768, i16)).toBeCloseTo(-5);
    expect(codeToUnits(0, i16)).toBeCloseTo(0);
  });
});

describe("identityCalibration", () => {
  it("shows raw codes unscaled", () => {
    expect(codeToUnits(200, identityCalibration(3))).toBe(200);
  });
});
