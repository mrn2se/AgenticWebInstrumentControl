import type { Data as PlotlyData, Layout as PlotlyLayout } from "plotly.js-dist-min";
import type { AnalogPanelSpec, Layout, PanelSpec, ThemeColors, Trace, TraceSet } from "./types";
import { analogPanelBounds } from "./webgl2-panel-transform";

/**
 * Pure `TraceSet`/`Layout` -> Plotly `Data`/`Layout` mapping, split out of
 * `plotly.ts` so it can be unit-tested without touching the real Plotly
 * library, a `<canvas>`, or any DOM (this project's vitest config runs
 * plain Node — see `vite.config.ts` — with no `document`).
 *
 * Panels are stacked with Plotly's `grid` feature: panel *i* (0-based) owns
 * axis pair `x`/`y` for `i === 0`, `x2`/`y2` for `i === 1`, and so on. Axis
 * overrides are assembled into a plain string-keyed record and cast once at
 * the boundary (Plotly's own `Layout` type expresses `xaxis2..xaxisN` via a
 * template-literal index signature, which is fiddlier to build incrementally
 * than it's worth here), capped at 9 panels — comfortably above any capture
 * this project's devices produce: at most 8 digital channels, or 2 analog.
 */
const MAX_PANELS = 9;

function axisSuffix(panelIndex: number): string {
  if (panelIndex < 0 || panelIndex >= MAX_PANELS) {
    throw new Error(`plotly renderer: panel index ${panelIndex} is outside the supported range 0..${MAX_PANELS - 1}`);
  }
  return panelIndex === 0 ? "" : String(panelIndex + 1);
}

/** The trace-level `xaxis`/`yaxis` reference for a panel, e.g. `"x"`, `"y3"`. */
export function xAxisRef(panelIndex: number): string {
  return `x${axisSuffix(panelIndex)}`;
}
export function yAxisRef(panelIndex: number): string {
  return `y${axisSuffix(panelIndex)}`;
}

function xAxisKey(panelIndex: number): string {
  return `xaxis${axisSuffix(panelIndex)}`;
}
function yAxisKey(panelIndex: number): string {
  return `yaxis${axisSuffix(panelIndex)}`;
}

function panelKey(kind: PanelSpec["kind"], channel: number): string {
  return `${kind}:${channel}`;
}

/** Maps `(kind, channel)` to its panel's index in `layout.panels`, in declaration order. */
export function panelIndex(layout: Layout): ReadonlyMap<string, number> {
  const map = new Map<string, number>();
  layout.panels.forEach((panel, i) => map.set(panelKey(panel.kind, panel.channel), i));
  return map;
}

function requirePanel(index: ReadonlyMap<string, number>, kind: PanelSpec["kind"], channel: number): number {
  const found = index.get(panelKey(kind, channel));
  if (found === undefined) {
    throw new Error(`plotly renderer: no panel declared for ${kind} channel ${channel}`);
  }
  return found;
}

const ANALOG_COLOR = "#2563eb";
const ENVELOPE_FILL = "rgba(37, 99, 235, 0.25)";
const DIGITAL_COLOR = "#1a7f37";
const TOGGLE_COLOR = "rgba(179, 38, 30, 0.55)";

/**
 * Each digital channel gets its own stacked grid row (SPEC §5.2: "stacked
 * lanes, one per channel") — a dedicated y-axis per panel, exactly like an
 * analog panel, rather than several channels sharing one axis. The row's
 * y-axis is fixed to this 0..1 range regardless of channel count.
 */
const DIGITAL_LANE_HEIGHT = 1;

function analogTraceToPlotly(trace: Extract<Trace, { kind: "analog" }>, panel: number): PlotlyData {
  return {
    type: "scattergl",
    mode: "lines",
    name: trace.label,
    x: [...trace.t],
    y: [...trace.v],
    xaxis: xAxisRef(panel),
    yaxis: yAxisRef(panel),
    line: { color: ANALOG_COLOR, width: 1.25 },
    hoverinfo: "x+y",
  };
}

/**
 * Renders a min/max envelope as a filled band: the min line, then the max
 * line with `fill: "tonexty"` back down to it. Bucket boundaries (`t1`) are
 * not separately plotted — adjacent buckets are assumed contiguous, which
 * holds for AWIC's decimated previews (SPEC §3.2) — so the band is drawn
 * against each bucket's `t0`.
 */
function analogEnvelopeToPlotly(
  trace: Extract<Trace, { kind: "analog-envelope" }>,
  panel: number,
): PlotlyData[] {
  const x = [...trace.t0];
  const xaxis = xAxisRef(panel);
  const yaxis = yAxisRef(panel);
  const minTrace: PlotlyData = {
    type: "scattergl",
    mode: "lines",
    name: `${trace.label} (min)`,
    x,
    y: [...trace.vMin],
    xaxis,
    yaxis,
    line: { color: ANALOG_COLOR, width: 0.5 },
    showlegend: false,
    hoverinfo: "skip",
  };
  const maxTrace: PlotlyData = {
    type: "scattergl",
    mode: "lines",
    name: trace.label,
    x,
    y: [...trace.vMax],
    xaxis,
    yaxis,
    line: { color: ANALOG_COLOR, width: 0.5 },
    fill: "tonexty",
    fillcolor: ENVELOPE_FILL,
    hoverinfo: "x+y",
  };
  return [minTrace, maxTrace];
}

/**
 * A digital channel becomes two overlaid traces: a level trace (the square
 * wave, `null`-separated between segments so gaps don't connect), and — only
 * when present — a toggle-marker trace of filled rectangles spanning the
 * lane's full height. The toggle rectangles are a distinct trace, not a
 * level, per SPEC §3.2: a `(0,1)` bucket means the channel changed state
 * inside it, which is not representable as "high" or "low".
 */
function digitalTraceToPlotly(trace: Extract<Trace, { kind: "digital" }>, panel: number): PlotlyData[] {
  const base = 0;
  const xaxis = xAxisRef(panel);
  const yaxis = yAxisRef(panel);

  const levelX: (number | null)[] = [];
  const levelY: (number | null)[] = [];
  const toggleX: (number | null)[] = [];
  const toggleY: (number | null)[] = [];

  for (const seg of trace.segments) {
    if (seg.state === "toggled") {
      toggleX.push(seg.t0, seg.t0, seg.t1, seg.t1, seg.t0, null);
      toggleY.push(base, base + DIGITAL_LANE_HEIGHT, base + DIGITAL_LANE_HEIGHT, base, base, null);
      // A break in the level trace too: the level is genuinely unknown here.
      levelX.push(null);
      levelY.push(null);
      continue;
    }
    const y = seg.state === "high" ? base + DIGITAL_LANE_HEIGHT : base;
    levelX.push(seg.t0, seg.t1);
    levelY.push(y, y);
  }

  const traces: PlotlyData[] = [
    {
      type: "scattergl",
      mode: "lines",
      name: trace.label,
      x: levelX,
      y: levelY,
      xaxis,
      yaxis,
      line: { color: DIGITAL_COLOR, width: 1.5, shape: "hv" },
      connectgaps: false,
      hoverinfo: "x+y",
    },
  ];

  if (toggleX.length > 0) {
    traces.push({
      type: "scattergl",
      mode: "lines",
      name: `${trace.label} (toggled)`,
      x: toggleX,
      y: toggleY,
      xaxis,
      yaxis,
      line: { color: TOGGLE_COLOR, width: 0 },
      fill: "toself",
      fillcolor: TOGGLE_COLOR,
      hoverinfo: "skip",
    });
  }

  return traces;
}

export function tracesToPlotlyData(traceSet: TraceSet, layout: Layout): PlotlyData[] {
  const index = panelIndex(layout);
  const data: PlotlyData[] = [];
  for (const trace of traceSet.traces) {
    const panel = requirePanel(index, trace.kind === "analog-envelope" ? "analog" : trace.kind, trace.channel);
    switch (trace.kind) {
      case "analog":
        data.push(analogTraceToPlotly(trace, panel));
        break;
      case "analog-envelope":
        data.push(...analogEnvelopeToPlotly(trace, panel));
        break;
      case "digital":
        data.push(...digitalTraceToPlotly(trace, panel));
        break;
      default: {
        const exhaustive: never = trace;
        throw new Error(`plotly renderer: unhandled trace kind ${JSON.stringify(exhaustive)}`);
      }
    }
  }
  return data;
}

/**
 * Extracts a user-driven time range from a `plotly_relayout` event payload
 * (SPEC §5.3's `onViewportChange`, implemented in `plotly.ts`). Plotly names
 * the changed keys after whichever x-axis the user actually dragged
 * (`"xaxis.range[0]"`, `"xaxis2.range[0]"`, ...); with every later panel's
 * x-axis `matches`-linked to the first (`buildPlotlyLayout` above), any one
 * of them reflects the same shared viewport, so the first pair found is
 * enough. `null` for an event that isn't a range change at all (e.g. a
 * legend click, or `xaxis.autorange` with no explicit bounds).
 */
export function relayoutTimeRange(event: Readonly<Record<string, unknown>>): { t0: number; t1: number } | null {
  for (const key of Object.keys(event)) {
    if (!/^xaxis\d*\.range\[0\]$/.test(key)) continue;
    const t0 = event[key];
    const t1 = event[key.replace("[0]", "[1]")];
    if (typeof t0 === "number" && typeof t1 === "number") return { t0, t1 };
  }
  return null;
}

/**
 * A lane covers `[offsetV - rangeV, offsetV + rangeV]`: SPEC §5.2's
 * `ranges_v` is a **± full-scale** range, not a total span (B-55). Shared with
 * the WebGL2 renderer so the two backends cannot drift apart on it.
 */
function analogAxisRange(panel: AnalogPanelSpec): [number, number] {
  const { min, max } = analogPanelBounds(panel.rangeV, panel.offsetV);
  return [min, max];
}

function digitalAxisRange(): [number, number] {
  return [-0.15, DIGITAL_LANE_HEIGHT + 0.15];
}

/**
 * Builds the Plotly `layout` object stacking one row per panel via Plotly's
 * `grid`, with each analog panel's y-axis ranged from the capture's
 * volts/div and offset (SPEC §5.2), the viewport (if set) applied to every
 * x-axis so pan/zoom stays in sync across panels, and — when `theme` is
 * given (SPEC §11.1) — the plot's own backgrounds, font and gridlines
 * following AWIC's current light/dark colors, since Plotly draws into an SVG
 * with its own inline styles rather than inheriting the page's CSS.
 */
export function buildPlotlyLayout(
  layout: Layout,
  viewport: { readonly t0: number; readonly t1: number } | null,
  theme?: ThemeColors,
): Partial<PlotlyLayout> {
  const panels = layout.panels;
  const result: Record<string, unknown> = {
    grid: { rows: Math.max(panels.length, 1), columns: 1, pattern: "independent" },
    showlegend: true,
    // Size comes from the container (a definite CSS height), never from here:
    // a layout height would fight the container and reintroduce feedback.
    autosize: true,
    margin: { l: 56, r: 16, t: 16, b: 32 },
  };

  if (theme) {
    result["paper_bgcolor"] = theme.background;
    result["plot_bgcolor"] = theme.background;
    result["font"] = { color: theme.foreground };
  }

  panels.forEach((panel, i) => {
    const axis: Record<string, unknown> = {
      title: { text: panel.label },
    };
    if (viewport) axis["range"] = [viewport.t0, viewport.t1];
    if (theme) {
      axis["gridcolor"] = theme.border;
      axis["linecolor"] = theme.border;
      axis["zerolinecolor"] = theme.border;
      axis["color"] = theme.mutedForeground;
    }
    // Every panel shares one time viewport (SPEC §5.3): panel 0's x-axis is
    // the reference, and every later panel's x-axis `matches` it, so a
    // drag-zoom on any panel pans/zooms all of them together, and
    // `onViewportChange` (`plotly.ts`) only ever needs to look at one axis's
    // range regardless of which panel the user actually dragged.
    if (i > 0) axis["matches"] = "x";
    result[xAxisKey(i)] = axis;

    if (panel.kind === "analog") {
      result[yAxisKey(i)] = { ...axis, range: analogAxisRange(panel), title: { text: "V" } };
    } else {
      result[yAxisKey(i)] = {
        ...axis,
        range: digitalAxisRange(),
        tickvals: [],
        title: { text: "" },
      };
    }
  });

  return result as Partial<PlotlyLayout>;
}
