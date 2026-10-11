import type { PlotlyHTMLElement, PlotRelayoutEvent } from "plotly.js-dist-min";
import { registerRenderer } from "./registry";
import { buildPlotlyLayout, relayoutTimeRange, tracesToPlotlyData } from "./plotly-mapping";
import type { AnnotationLane, Layout, ThemeColors, TraceSet, WaveformRenderer } from "./types";

/**
 * `plotly.js-dist-min` ships its own native TypeScript types (a genuine
 * `export default`, unlike an older `export =` CJS shape), so the dynamically
 * imported module's `.default` is already precisely typed against the real
 * `newPlot`/`react`/`relayout`/`purge` signatures — no hand-rolled interface
 * or `any`/`unknown` cast needed.
 */
type PlotlyStatic = typeof import("plotly.js-dist-min")["default"];

/**
 * SPEC §5.3, R-06: Plotly is the default renderer, loaded via dynamic
 * `import()` so it is code-split out of the initial bundle (verified in the
 * build output — see the task report) and fetched only once the first plot
 * mounts. This module itself has no top-level dependency on
 * `plotly.js-dist-min` — only `mount()` triggers the import — which is what
 * keeps it out of the entry chunk's static dependency graph.
 */
let plotlyModulePromise: Promise<PlotlyStatic> | null = null;

function loadPlotly(): Promise<PlotlyStatic> {
  plotlyModulePromise ??= import("plotly.js-dist-min").then((mod) => mod.default);
  return plotlyModulePromise;
}

interface Viewport {
  readonly t0: number;
  readonly t1: number;
}

export class PlotlyRenderer implements WaveformRenderer {
  readonly id = "plotly" as const;

  private el: HTMLElement | null = null;
  private layout: Layout | null = null;
  private plotly: PlotlyStatic | null = null;
  private mountToken = 0;
  private pendingTraces: TraceSet = { traces: [] };
  private viewport: Viewport | null = null;
  private themeColors: ThemeColors | null = null;
  private viewportListeners = new Set<(t0: number, t1: number) => void>();
  /**
   * Set around every `relayout`/`react`/`newPlot` call *we* make, so the
   * `plotly_relayout` event those calls themselves fire doesn't loop back
   * into `onViewportChange` — that callback is for the user zooming or
   * panning the plot directly (SPEC §5.3's doc comment), not an echo of our
   * own `setViewport()`.
   */
  private suppressRelayoutEcho = false;

  private readonly handleRelayout = (event: PlotRelayoutEvent): void => {
    if (this.suppressRelayoutEcho) return;
    const range = relayoutTimeRange(event as Readonly<Record<string, unknown>>);
    if (!range) return;
    this.viewport = range;
    for (const cb of this.viewportListeners) cb(range.t0, range.t1);
  };

  mount(el: HTMLElement, layout: Layout): void {
    this.el = el;
    this.layout = layout;
    this.plotly = null;
    const token = ++this.mountToken;

    void loadPlotly().then((plotly) => {
      // Ignore a stale load: `destroy()` or a second `mount()` happened
      // while the dynamic import was in flight.
      if (token !== this.mountToken || this.el !== el) return;
      this.plotly = plotly;
      const data = tracesToPlotlyData(this.pendingTraces, layout);
      const plotlyLayout = buildPlotlyLayout(layout, this.viewport, this.themeColors ?? undefined);
      this.suppressRelayoutEcho = true;
      void plotly
        .newPlot(el, data, plotlyLayout, { responsive: true, displaylogo: false })
        .then((graphDiv: PlotlyHTMLElement) => {
          if (token !== this.mountToken || this.el !== el) return;
          graphDiv.on("plotly_relayout", this.handleRelayout);
        })
        .finally(() => {
          this.suppressRelayoutEcho = false;
        });
    });
  }

  /**
   * A new panel layout under the data already plotted (B-55): the lanes'
   * ranges, offsets or membership changed. `react` is Plotly's own
   * "same plot, new spec" call, so this is a redraw rather than the
   * `newPlot` a re-`mount()` would do.
   */
  setLayout(layout: Layout): void {
    this.layout = layout;
    if (!this.plotly || !this.el) return;
    const data = tracesToPlotlyData(this.pendingTraces, layout);
    const plotlyLayout = buildPlotlyLayout(layout, this.viewport, this.themeColors ?? undefined);
    this.suppressRelayoutEcho = true;
    void this.plotly
      .react(this.el, data, plotlyLayout, { responsive: true, displaylogo: false })
      .finally(() => {
        this.suppressRelayoutEcho = false;
      });
  }

  setTraces(traces: TraceSet): void {
    this.pendingTraces = traces;
    if (!this.plotly || !this.el || !this.layout) return;
    const data = tracesToPlotlyData(traces, this.layout);
    const plotlyLayout = buildPlotlyLayout(this.layout, this.viewport, this.themeColors ?? undefined);
    void this.plotly.react(this.el, data, plotlyLayout, { responsive: true, displaylogo: false });
  }

  /**
   * Plotly has no annotation-lane concept (SPEC §5.3: it "stays selectable"
   * but `webgl2` is where §6/T21's decode overlay is actually drawn) — a
   * deliberate no-op so the graph module can call this unconditionally
   * without checking which renderer is mounted.
   */
  setAnnotations(_lanes: readonly AnnotationLane[]): void {
    // Intentionally empty.
  }

  setViewport(t0: number, t1: number): void {
    this.viewport = { t0, t1 };
    if (!this.plotly || !this.el || !this.layout) return;
    const plotlyLayout = buildPlotlyLayout(this.layout, this.viewport, this.themeColors ?? undefined);
    this.suppressRelayoutEcho = true;
    void this.plotly.relayout(this.el, plotlyLayout).finally(() => {
      this.suppressRelayoutEcho = false;
    });
  }

  /** SPEC §5.3. Returns an unsubscribe function; safe to call before `mount()`'s dynamic import resolves. */
  onViewportChange(cb: (t0: number, t1: number) => void): () => void {
    this.viewportListeners.add(cb);
    return () => this.viewportListeners.delete(cb);
  }

  /**
   * Applies (or re-applies, on a light/dark switch) the plot's colors (SPEC
   * §11.1). Safe to call before the dynamic Plotly import resolves — the
   * colors are stored and picked up by `mount()`'s eventual `newPlot` — and
   * a no-op once `destroy()`s reset them until the next `mount()`.
   */
  setTheme(colors: ThemeColors): void {
    this.themeColors = colors;
    if (!this.plotly || !this.el || !this.layout) return;
    const plotlyLayout = buildPlotlyLayout(this.layout, this.viewport, this.themeColors);
    this.suppressRelayoutEcho = true;
    void this.plotly.relayout(this.el, plotlyLayout).finally(() => {
      this.suppressRelayoutEcho = false;
    });
  }

  destroy(): void {
    this.mountToken++; // invalidate any in-flight mount() load
    if (this.plotly && this.el) this.plotly.purge(this.el); // also removes the `plotly_relayout` listener
    this.el = null;
    this.layout = null;
    this.plotly = null;
    this.pendingTraces = { traces: [] };
    this.viewport = null;
    this.themeColors = null;
    this.viewportListeners.clear();
  }
}

registerRenderer("plotly", () => new PlotlyRenderer());
