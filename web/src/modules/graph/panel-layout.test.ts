import { describe, expect, it } from "vitest";
import { buildLayout, controlFromRecChannel } from "./panel-layout";
import type { ChannelControl } from "./capabilities";
import type { RecChannel } from "../../proto/types";

describe("buildLayout", () => {
  it("builds one panel per enabled control, analog panels carrying the chosen range/offset", () => {
    const controls: ChannelControl[] = [
      { kind: "analog", channel: 0, label: "CH1", bits: 8, ratesHz: [], rangesV: [5, 10] },
      { kind: "digital", channel: 3, label: "D3" },
    ];
    const layout = buildLayout(controls, () => ({ rangeV: 10, offsetV: 1 }));
    expect(layout.panels).toEqual([
      { kind: "analog", channel: 0, label: "CH1", rangeV: 10, offsetV: 1 },
      { kind: "digital", channel: 3, label: "D3", lane: 1 },
    ]);
  });

  it("assigns digital lane numbers by position among enabled controls", () => {
    const controls: ChannelControl[] = [
      { kind: "digital", channel: 0, label: "D0" },
      { kind: "digital", channel: 1, label: "D1" },
    ];
    const layout = buildLayout(controls, () => ({ rangeV: 5, offsetV: 0 }));
    expect(layout.panels.map((p) => (p.kind === "digital" ? p.lane : -1))).toEqual([0, 1]);
  });

  it("produces no panels for an empty control list", () => {
    expect(buildLayout([], () => ({ rangeV: 5, offsetV: 0 }))).toEqual({ panels: [] });
  });
});

describe("controlFromRecChannel", () => {
  function recChannel(overrides: Partial<RecChannel> = {}): RecChannel {
    return { channel: 0, name: "D0", calibration: { channel: 0, scale: 1, offset: 0, unit: "" }, ...overrides };
  }

  it("reads a digital control from a D<n> name", () => {
    expect(controlFromRecChannel(recChannel({ channel: 3, name: "D3" }))).toEqual({
      kind: "digital",
      channel: 3,
      label: "D3",
    });
  });

  it("reads an analog control from a CH<n> name", () => {
    const control = controlFromRecChannel(recChannel({ channel: 0, name: "CH1" }));
    expect(control.kind).toBe("analog");
    expect(control.channel).toBe(0);
    expect(control.label).toBe("CH1");
  });

  it("round-trips through buildLayout with the caller's chosen range/offset", () => {
    const channels: RecChannel[] = [recChannel({ channel: 0, name: "CH1" }), recChannel({ channel: 1, name: "D1" })];
    const layout = buildLayout(channels.map(controlFromRecChannel), () => ({ rangeV: 2.5, offsetV: 0 }));
    expect(layout.panels).toEqual([
      { kind: "analog", channel: 0, label: "CH1", rangeV: 2.5, offsetV: 0 },
      { kind: "digital", channel: 1, label: "D1", lane: 1 },
    ]);
  });
});
