import { describe, expect, it } from "vitest";
import type { CapabilityInfo } from "../../proto/types";
import { buildControlSchema, controlsFingerprint, isAnalogControl, isDigitalControl } from "./capabilities";

describe("buildControlSchema", () => {
  it("expands an analog capability into one control per channel, 0-indexed", () => {
    const caps: CapabilityInfo[] = [
      { kind: "analog", channels: 2, bits: 8, rates_hz: [24_000_000, 8_000_000], ranges_v: [5, 10, 20] },
    ];
    const schema = buildControlSchema(caps);
    expect(schema.channels).toEqual([
      { kind: "analog", channel: 0, label: "CH1", bits: 8, ratesHz: [24_000_000, 8_000_000], rangesV: [5, 10, 20] },
      { kind: "analog", channel: 1, label: "CH2", bits: 8, ratesHz: [24_000_000, 8_000_000], rangesV: [5, 10, 20] },
    ]);
    expect(schema.sampleRatesHz).toEqual([24_000_000, 8_000_000]);
  });

  it("expands a digital capability into one control per channel, labelled Dn", () => {
    const caps: CapabilityInfo[] = [{ kind: "digital", channels: 8, max_rate_hz: 24_000_000, rates_hz: [] }];
    const schema = buildControlSchema(caps);
    expect(schema.channels).toHaveLength(8);
    expect(schema.channels[0]).toEqual({ kind: "digital", channel: 0, label: "D0" });
    expect(schema.channels[7]).toEqual({ kind: "digital", channel: 7, label: "D7" });
  });

  it("offers only the ceiling rate for a digital-only device (no ladder on the wire)", () => {
    const caps: CapabilityInfo[] = [{ kind: "digital", channels: 8, max_rate_hz: 24_000_000, rates_hz: [] }];
    const schema = buildControlSchema(caps);
    expect(schema.sampleRatesHz).toEqual([24_000_000]);
  });

  it("offers the driver's exact rate ladder for a digital device that reports one (pick 1 MHz for the logic analyzer)", () => {
    const caps: CapabilityInfo[] = [
      { kind: "digital", channels: 8, max_rate_hz: 24_000_000, rates_hz: [20_000, 1_000_000, 24_000_000] },
    ];
    const schema = buildControlSchema(caps);
    expect(schema.sampleRatesHz).toEqual([20_000, 1_000_000, 24_000_000]);
    expect(schema.sampleRatesHz).toContain(1_000_000);
  });

  it("ignores a power_supply capability entirely (PSU module's concern)", () => {
    const caps: CapabilityInfo[] = [{ kind: "power_supply", channels: 2, v_max: 30, i_max: 3 }];
    const schema = buildControlSchema(caps);
    expect(schema.channels).toEqual([]);
    expect(schema.sampleRatesHz).toEqual([]);
  });

  it("produces no controls for an empty capability list", () => {
    expect(buildControlSchema([])).toEqual({ channels: [], sampleRatesHz: [] });
  });

  it("is capability-driven: an unfamiliar analog channel count still produces a full control set", () => {
    // Stands in for "a new driver gets a working UI for free" (SPEC §5.2) —
    // nothing here special-cases a channel count or model.
    const caps: CapabilityInfo[] = [{ kind: "analog", channels: 4, bits: 12, rates_hz: [1_000_000], ranges_v: [2] }];
    const schema = buildControlSchema(caps);
    expect(schema.channels).toHaveLength(4);
    expect(schema.channels.every(isAnalogControl)).toBe(true);
  });
});

describe("type guards", () => {
  it("narrow analog vs digital controls", () => {
    const analog = { kind: "analog", channel: 0, label: "CH1", bits: 8, ratesHz: [], rangesV: [] } as const;
    const digital = { kind: "digital", channel: 0, label: "D0" } as const;
    expect(isAnalogControl(analog)).toBe(true);
    expect(isDigitalControl(analog)).toBe(false);
    expect(isDigitalControl(digital)).toBe(true);
    expect(isAnalogControl(digital)).toBe(false);
  });
});

// B-65: a Start makes the device go busy then ready, and every one of those
// store updates rebuilt the Graph tab's controls — resetting the rate to the
// device's fastest, the ranges to defaults and the trigger to channel 0. Only
// a different device, or different capabilities, is a reason to rebuild.
describe("controlsFingerprint", () => {
  const scope: CapabilityInfo[] = [
    { kind: "analog", channels: 2, bits: 8, rates_hz: [16_000_000, 8_000_000], ranges_v: [5, 2.5] },
  ];

  it("is unchanged when only the device's state changed", () => {
    // Two store updates for the same device: `capabilities` is all this reads,
    // so a busy/ready transition cannot move it.
    expect(controlsFingerprint("3-3.4.2", scope)).toBe(controlsFingerprint("3-3.4.2", scope));
    // A fresh array with equal contents is still the same fingerprint — the
    // store hands out new objects on every update.
    const copy: CapabilityInfo[] = [
      { kind: "analog", channels: 2, bits: 8, rates_hz: [16_000_000, 8_000_000], ranges_v: [5, 2.5] },
    ];
    expect(controlsFingerprint("3-3.4.2", copy)).toBe(controlsFingerprint("3-3.4.2", scope));
  });

  it("changes when the selected device changes", () => {
    expect(controlsFingerprint("3-3.4.1.1", scope)).not.toBe(controlsFingerprint("3-3.4.2", scope));
  });

  it("changes when the same device reports different capabilities", () => {
    const switched: CapabilityInfo[] = [{ kind: "digital", channels: 8, max_rate_hz: 24_000_000, rates_hz: [] }];
    expect(controlsFingerprint("3-3.4.2", switched)).not.toBe(controlsFingerprint("3-3.4.2", scope));
    // The oscilloscope's mode switch (SPEC §5.1) is exactly this case, and it must
    // rebuild: the channels it offers have changed.
    const narrower: CapabilityInfo[] = [
      { kind: "analog", channels: 2, bits: 8, rates_hz: [8_000_000], ranges_v: [5, 2.5] },
    ];
    expect(controlsFingerprint("3-3.4.2", narrower)).not.toBe(controlsFingerprint("3-3.4.2", scope));
  });

  it("handles no device and no capabilities without throwing", () => {
    expect(controlsFingerprint(null, undefined)).toBe(controlsFingerprint(null, []));
    expect(controlsFingerprint(null, undefined)).not.toBe(controlsFingerprint("3-3.4.2", scope));
  });
});
