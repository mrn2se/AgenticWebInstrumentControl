import { describe, expect, it } from "vitest";
import type { SimStatus } from "../../proto/types";
import {
  agentPrompts,
  channelName,
  formatUptime,
  instrumentShortName,
  r2ToSlider,
  sliderToR2,
  testPointRows,
} from "./bench-model";
import { litSegments } from "./seven-seg";

function status(): SimStatus {
  return {
    seed: 1,
    hub: "254-1",
    board: {
      name: "b",
      description: "d",
      device: "254-1.1",
      port: 1,
      powered: true,
      uptime_s: 1,
      boots: 1,
      last_reset_cause: "power-on",
      serial_source: "serial:254-1.1:1.0",
      console: "115200 8N1",
      test_points: [
        { id: "TP1", name: "UART TX", description: "", kind: "digital" },
        { id: "TP9", name: "555 OUT", description: "", kind: "digital" },
        { id: "TP10", name: "555 CAP", description: "", kind: "analog" },
      ],
    },
    counter: {
      name: "U3",
      description: "",
      solved: false,
      wrong_guesses: 0,
      inputs: [],
      display: "",
    },
    instruments: [
      {
        device: "254-1.2",
        label: "AWIC virtual logic analyzer (8 channels)",
        port: 2,
        powered: true,
        probes: [{ channel: 0 }, { channel: 1 }],
        probed_points: ["TP1", "TP9"],
      },
      {
        device: "254-1.3",
        label: "AWIC virtual oscilloscope (2 channels)",
        port: 3,
        powered: true,
        probes: [
          { channel: 0, test_point: "TP10" },
          { channel: 1, test_point: "TP9" },
        ],
        probed_points: ["TP9", "TP10"],
      },
    ],
    faults: [],
    mystery: false,
    revealed: false,
    osc_r2_ohm: 6800,
    osc_nominal_hz: 988,
  };
}

describe("testPointRows", () => {
  it("puts each known probe on its test point and counts the secret ones", () => {
    const { rows, hiddenProbes } = testPointRows(status());
    expect(hiddenProbes).toBe(2);
    expect(rows.find((r) => r.id === "TP10")?.probes).toEqual(["Scope CH1"]);
    expect(rows.find((r) => r.id === "TP9")?.probes).toEqual(["Logic #1 D?", "Scope CH2"]);
    expect(rows.find((r) => r.id === "TP1")?.probes).toEqual(["Logic #1 D?"]);
  });
});

describe("names", () => {
  it("tells the two analyzers and the scope apart", () => {
    expect(instrumentShortName("AWIC virtual logic analyzer #2 (8 channels)")).toBe("Logic #2");
    expect(instrumentShortName("AWIC virtual logic analyzer (8 channels)")).toBe("Logic #1");
    expect(channelName("AWIC virtual oscilloscope (2 channels)", 0)).toBe("CH1");
    expect(channelName("AWIC virtual logic analyzer (8 channels)", 3)).toBe("D3");
  });
});

describe("formatUptime", () => {
  it("reads like a person would say it", () => {
    expect(formatUptime(null)).toBe("not running");
    expect(formatUptime(12.34)).toBe("12.3 s");
    expect(formatUptime(245)).toBe("4 min 05 s");
    expect(formatUptime(3720)).toBe("1 h 02 min");
  });
});

describe("the pot slider", () => {
  it("round-trips the default R2 closely", () => {
    const back = sliderToR2(r2ToSlider(6800));
    expect(Math.abs(back - 6800) / 6800).toBeLessThan(0.02);
  });

  it("spans the pot's range", () => {
    expect(sliderToR2(0)).toBe(470);
    expect(sliderToR2(1000)).toBe(100_000);
  });
});

describe("litSegments", () => {
  it("keeps only segment letters", () => {
    expect([...litSegments("cdefg")].sort()).toEqual(["c", "d", "e", "f", "g"]);
    expect(litSegments("").size).toBe(0);
    expect(litSegments("xz").size).toBe(0);
  });
});

describe("agentPrompts", () => {
  it("names the real devices and never the secrets", () => {
    const s = status();
    s.instruments.push({
      device: "254-1.4",
      label: "AWIC virtual logic analyzer #2 (8 channels)",
      port: 4,
      powered: true,
      probes: [],
      probed_points: [],
    });
    const prompts = agentPrompts(s);
    expect(prompts.length).toBeGreaterThan(3);
    const all = prompts.map((p) => p.text).join("\n");
    expect(all).toContain("254-1.2");
    expect(all).toContain("254-1.3");
    expect(all).toContain("254-1.4");
    expect(all).toContain("serial:254-1.1:1.0");
    expect(all).not.toMatch(/4026/);
    for (const p of prompts) expect(p.yourPart.length).toBeGreaterThan(0);
  });
});
