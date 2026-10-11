import { describe, expect, it } from "vitest";
import { buildPlotlyLayout, panelIndex, relayoutTimeRange, tracesToPlotlyData, xAxisRef, yAxisRef } from "./plotly-mapping";
import type { Layout, TraceSet } from "./types";

describe("panelIndex / axis refs", () => {
  const layout: Layout = {
    panels: [
      { kind: "analog", channel: 0, label: "CH1", rangeV: 5, offsetV: 0 },
      { kind: "digital", channel: 3, label: "D3", lane: 0 },
    ],
  };

  it("maps each panel to its declaration-order index", () => {
    const idx = panelIndex(layout);
    expect(idx.get("analog:0")).toBe(0);
    expect(idx.get("digital:3")).toBe(1);
  });

  it("names the first panel's axes without a suffix, later ones with 2, 3, ...", () => {
    expect(xAxisRef(0)).toBe("x");
    expect(yAxisRef(0)).toBe("y");
    expect(xAxisRef(1)).toBe("x2");
    expect(yAxisRef(1)).toBe("y2");
  });

  it("rejects a panel index beyond the supported maximum", () => {
    expect(() => xAxisRef(9)).toThrow(/outside the supported range/);
  });
});

describe("tracesToPlotlyData", () => {
  const layout: Layout = {
    panels: [
      { kind: "analog", channel: 0, label: "CH1", rangeV: 5, offsetV: 0 },
      { kind: "digital", channel: 0, label: "D0", lane: 0 },
    ],
  };

  it("maps an analog trace onto its panel's axis pair", () => {
    const traces: TraceSet = {
      traces: [{ kind: "analog", channel: 0, label: "CH1", t: [0, 1, 2], v: [0.1, 0.2, 0.3] }],
    };
    const data = tracesToPlotlyData(traces, layout);
    expect(data).toHaveLength(1);
    expect(data[0]).toMatchObject({ type: "scattergl", xaxis: "x", yaxis: "y", x: [0, 1, 2], y: [0.1, 0.2, 0.3] });
  });

  it("expands an analog envelope into a min/max band pair on the same axis", () => {
    const traces: TraceSet = {
      traces: [
        {
          kind: "analog-envelope",
          channel: 0,
          label: "CH1",
          t0: [0, 1],
          t1: [1, 2],
          vMin: [-1, -2],
          vMax: [1, 2],
        },
      ],
    };
    const data = tracesToPlotlyData(traces, layout);
    expect(data).toHaveLength(2);
    expect(data[0]).toMatchObject({ y: [-1, -2], xaxis: "x", yaxis: "y" });
    expect(data[1]).toMatchObject({ y: [1, 2], fill: "tonexty" });
  });

  it("draws digital level segments as a stepped line with no toggle overlay when nothing toggled", () => {
    const traces: TraceSet = {
      traces: [
        {
          kind: "digital",
          channel: 0,
          label: "D0",
          lane: 0,
          segments: [
            { t0: 0, t1: 1, state: "low" },
            { t0: 1, t1: 2, state: "high" },
          ],
        },
      ],
    };
    const data = tracesToPlotlyData(traces, layout);
    expect(data).toHaveLength(1);
    expect(data[0]).toMatchObject({ xaxis: "x2", yaxis: "y2" });
  });

  it("adds a distinct filled toggle-marker trace for a toggled digital segment, never a level", () => {
    const traces: TraceSet = {
      traces: [
        {
          kind: "digital",
          channel: 0,
          label: "D0",
          lane: 0,
          segments: [
            { t0: 0, t1: 1, state: "low" },
            { t0: 1, t1: 2, state: "toggled" },
          ],
        },
      ],
    };
    const data = tracesToPlotlyData(traces, layout);
    // One level trace, plus one toggle-marker trace.
    expect(data).toHaveLength(2);
    const toggleTrace = data[1];
    expect(toggleTrace).toMatchObject({ fill: "toself" });
    // Spans the toggled bucket's full time range and lane height (0..1), never a mid-height level.
    expect(toggleTrace).toMatchObject({
      x: expect.arrayContaining([1, 2]),
      y: expect.arrayContaining([0, 1]),
    });
  });

  it("throws a clear error when a trace references a channel with no declared panel", () => {
    const traces: TraceSet = {
      traces: [{ kind: "analog", channel: 5, label: "CH6", t: [0], v: [0] }],
    };
    expect(() => tracesToPlotlyData(traces, layout)).toThrow(/no panel declared/);
  });
});

describe("buildPlotlyLayout", () => {
  it("never sets a height, so the plot cannot feed its own size back into its container", () => {
    const layout = buildPlotlyLayout({ panels: [] } as unknown as Parameters<typeof buildPlotlyLayout>[0], null);
    expect(layout.height).toBeUndefined();
    expect(layout.autosize).toBe(true);
  });

  // SPEC §5.2: a range setting is a ± full-scale range, so a lane spans twice
  // it. Halving it drew every analog lane half as tall as the hardware range
  // it was showing (B-55).
  it("ranges an analog panel's y-axis a full ± rangeV around offsetV", () => {
    const layout: Layout = { panels: [{ kind: "analog", channel: 0, label: "CH1", rangeV: 10, offsetV: 2 }] };
    const plotlyLayout = buildPlotlyLayout(layout, null) as Record<string, { range?: [number, number] }>;
    expect(plotlyLayout["yaxis"]?.range).toEqual([-8, 12]);
  });

  // The bench case: CANL/CANH at 2.7-4.1 V on the oscilloscope's 5 V range, where
  // code 255 is +5 V. Both lines have to be inside their own lane.
  it("fits a 2.7-4.1 V bus inside the 5 V range's lane", () => {
    const layout: Layout = {
      panels: [
        { kind: "analog", channel: 0, label: "CH1", rangeV: 5, offsetV: 0 },
        { kind: "analog", channel: 1, label: "CH2", rangeV: 5, offsetV: 0 },
      ],
    };
    const plotlyLayout = buildPlotlyLayout(layout, null) as Record<string, { range?: [number, number] }>;
    for (const axis of ["yaxis", "yaxis2"]) {
      const range = plotlyLayout[axis]?.range;
      expect(range).toBeDefined();
      const [min, max] = range!;
      expect(min).toBeLessThanOrEqual(2.7);
      expect(max).toBeGreaterThanOrEqual(4.1);
    }
  });

  it("an offset moves the lane it is set on", () => {
    const centred: Layout = { panels: [{ kind: "analog", channel: 0, label: "CH1", rangeV: 5, offsetV: 0 }] };
    const shifted: Layout = { panels: [{ kind: "analog", channel: 0, label: "CH1", rangeV: 5, offsetV: 2.5 }] };
    const a = buildPlotlyLayout(centred, null) as Record<string, { range?: [number, number] }>;
    const b = buildPlotlyLayout(shifted, null) as Record<string, { range?: [number, number] }>;
    expect(a["yaxis"]?.range).toEqual([-5, 5]);
    expect(b["yaxis"]?.range).toEqual([-2.5, 7.5]);
  });

  it("applies the viewport to every panel's x-axis", () => {
    const layout: Layout = {
      panels: [
        { kind: "analog", channel: 0, label: "CH1", rangeV: 5, offsetV: 0 },
        { kind: "digital", channel: 0, label: "D0", lane: 0 },
      ],
    };
    const plotlyLayout = buildPlotlyLayout(layout, { t0: 1, t1: 2 }) as Record<
      string,
      { range?: [number, number] }
    >;
    expect(plotlyLayout["xaxis"]?.range).toEqual([1, 2]);
    expect(plotlyLayout["xaxis2"]?.range).toEqual([1, 2]);
  });

  it("applies no theme colors at all when none is given", () => {
    const layout: Layout = { panels: [{ kind: "analog", channel: 0, label: "CH1", rangeV: 5, offsetV: 0 }] };
    const plotlyLayout = buildPlotlyLayout(layout, null) as Record<string, unknown>;
    expect(plotlyLayout["paper_bgcolor"]).toBeUndefined();
    expect(plotlyLayout["plot_bgcolor"]).toBeUndefined();
    expect(plotlyLayout["font"]).toBeUndefined();
  });

  it("follows the given theme's colors for paper/plot background, font and gridlines (SPEC §11.1)", () => {
    const layout: Layout = {
      panels: [
        { kind: "analog", channel: 0, label: "CH1", rangeV: 5, offsetV: 0 },
        { kind: "digital", channel: 0, label: "D0", lane: 0 },
      ],
    };
    const theme = { background: "#111111", foreground: "#eeeeee", mutedForeground: "#999999", border: "#333333" };
    const plotlyLayout = buildPlotlyLayout(layout, null, theme) as Record<string, unknown>;
    expect(plotlyLayout["paper_bgcolor"]).toBe("#111111");
    expect(plotlyLayout["plot_bgcolor"]).toBe("#111111");
    expect(plotlyLayout["font"]).toEqual({ color: "#eeeeee" });

    const xaxis = plotlyLayout["xaxis"] as Record<string, unknown>;
    const yaxis = plotlyLayout["yaxis"] as Record<string, unknown>;
    const xaxis2 = plotlyLayout["xaxis2"] as Record<string, unknown>;
    for (const axis of [xaxis, yaxis, xaxis2]) {
      expect(axis["gridcolor"]).toBe("#333333");
      expect(axis["linecolor"]).toBe("#333333");
      expect(axis["zerolinecolor"]).toBe("#333333");
      expect(axis["color"]).toBe("#999999");
    }
  });

  it("re-applying a different theme changes the colors without touching ranges", () => {
    const layout: Layout = { panels: [{ kind: "analog", channel: 0, label: "CH1", rangeV: 10, offsetV: 2 }] };
    const dark = buildPlotlyLayout(layout, null, {
      background: "#000",
      foreground: "#fff",
      mutedForeground: "#aaa",
      border: "#222",
    }) as Record<string, { range?: [number, number] }>;
    const light = buildPlotlyLayout(layout, null, {
      background: "#fff",
      foreground: "#000",
      mutedForeground: "#555",
      border: "#ddd",
    }) as Record<string, { range?: [number, number] }>;
    expect(dark["yaxis"]?.range).toEqual([-8, 12]);
    expect(light["yaxis"]?.range).toEqual([-8, 12]);
  });

  it("stacks one grid row per panel", () => {
    const layout: Layout = {
      panels: [
        { kind: "analog", channel: 0, label: "CH1", rangeV: 5, offsetV: 0 },
        { kind: "analog", channel: 1, label: "CH2", rangeV: 5, offsetV: 0 },
        { kind: "digital", channel: 0, label: "D0", lane: 0 },
      ],
    };
    const plotlyLayout = buildPlotlyLayout(layout, null) as { grid?: { rows: number; columns: number } };
    expect(plotlyLayout.grid).toMatchObject({ rows: 3, columns: 1 });
  });

  it("links every later panel's x-axis to the first, so one shared viewport zooms every panel together", () => {
    const layout: Layout = {
      panels: [
        { kind: "analog", channel: 0, label: "CH1", rangeV: 5, offsetV: 0 },
        { kind: "digital", channel: 0, label: "D0", lane: 0 },
      ],
    };
    const plotlyLayout = buildPlotlyLayout(layout, null) as Record<string, { matches?: string }>;
    expect(plotlyLayout["xaxis"]?.matches).toBeUndefined();
    expect(plotlyLayout["xaxis2"]?.matches).toBe("x");
  });
});

describe("relayoutTimeRange", () => {
  it("reads a range change off the first axis it finds", () => {
    expect(relayoutTimeRange({ "xaxis.range[0]": 1, "xaxis.range[1]": 2 })).toEqual({ t0: 1, t1: 2 });
  });

  it("reads a range change reported against a later panel's matched axis", () => {
    expect(relayoutTimeRange({ "xaxis2.range[0]": 3, "xaxis2.range[1]": 4 })).toEqual({ t0: 3, t1: 4 });
  });

  it("returns null for an event that carries no numeric range (e.g. an autorange toggle)", () => {
    expect(relayoutTimeRange({ "xaxis.autorange": true })).toBeNull();
  });

  it("returns null for an unrelated relayout (e.g. a legend/annotation change)", () => {
    expect(relayoutTimeRange({ "annotations[0].text": "x" })).toBeNull();
  });
});
