import { describe, expect, it } from "vitest";
import { buildAcqConfig, triggerSampleIndex } from "./acq-config";

describe("buildAcqConfig", () => {
  it("omits the trigger when disabled", () => {
    const cfg = buildAcqConfig({
      rateHz: 1_000_000,
      samples: 1024,
      enabledChannels: [0, 1],
      trigger: { enabled: false, channel: 0, slope: "rising", level: 0 },
    });
    expect(cfg).toEqual({ rate_hz: 1_000_000, channels: [0, 1], samples: 1024, trigger: null });
  });

  it("includes an edge trigger with the selected channel/slope/level", () => {
    const cfg = buildAcqConfig({
      rateHz: 24_000_000,
      samples: 4096,
      enabledChannels: [0],
      trigger: { enabled: true, channel: 0, slope: "falling", level: 1.5 },
    });
    expect(cfg.trigger).toEqual({ kind: "edge", channel: 0, slope: "falling", level: 1.5 });
  });
});

describe("triggerSampleIndex", () => {
  it("lands at the capture midpoint", () => {
    expect(triggerSampleIndex(1024)).toBe(512);
    expect(triggerSampleIndex(1001)).toBe(500);
  });
});

describe("buildAcqConfig ranges", () => {
  const base = {
    rateHz: 1_000_000,
    samples: 1000,
    enabledChannels: [0, 1],
    trigger: { enabled: false, channel: 0, slope: "rising" as const, level: 0 },
  };

  it("sends the selected ± range per analog channel — the only way to set a scope's gain", () => {
    expect(buildAcqConfig({ ...base, rangesV: [5, 0.5] }).ranges_v).toEqual([5, 0.5]);
  });

  it("omits ranges entirely for digital devices", () => {
    expect("ranges_v" in buildAcqConfig(base)).toBe(false);
  });
});
