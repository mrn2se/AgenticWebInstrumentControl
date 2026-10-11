/**
 * The renderer-facing vocabulary (SPECIFICATION.md §5.3). Everything here is
 * generic waveform data — seconds and volts, panel positions, digital lane
 * numbers — with no knowledge of the AWIC wire protocol. `modules/graph`
 * turns decoded frames (`modules/graph/decode.ts`) into these shapes; a
 * `WaveformRenderer` backend turns these shapes into pixels. Neither side
 * needs to know about the other's internals, which is the point of the
 * abstraction: a new backend (`webgl2`, `uplot`) or a new device driver
 * changes nothing here.
 */

/** One analog channel's full-resolution samples, in volts against time. */
export interface AnalogTrace {
  readonly kind: "analog";
  readonly channel: number;
  readonly label: string;
  /** Seconds, capture-relative. Same length as `v`. */
  readonly t: readonly number[];
  readonly v: readonly number[];
}

/**
 * One analog channel's decimated min/max envelope (SPEC §3.2 encodings 3/4).
 * Each bucket `i` spans `[t0[i], t1[i])` and is drawn as a filled band
 * between `vMin[i]` and `vMax[i]` — never collapsed to a single value, or a
 * glitch that decimation caught would be thrown away by the renderer after
 * the decoder took pains to preserve it.
 */
export interface AnalogEnvelopeTrace {
  readonly kind: "analog-envelope";
  readonly channel: number;
  readonly label: string;
  readonly t0: readonly number[];
  readonly t1: readonly number[];
  readonly vMin: readonly number[];
  readonly vMax: readonly number[];
}

/**
 * One digital channel's state over `[t0, t1)`. `"toggled"` means the channel
 * changed state one or more times inside this span and must never be drawn
 * as a level — it is the decimated case (SPEC §3.2 encoding 5, AND/OR bucket
 * with differing bits) where the exact transition times were lost on
 * purpose to bound bandwidth, but the fact that *something happened* here
 * must not be.
 */
export interface DigitalSegment {
  readonly t0: number;
  readonly t1: number;
  readonly state: "low" | "high" | "toggled";
}

/** One digital channel's lane, stacked with its siblings by `lane` index. */
export interface DigitalTrace {
  readonly kind: "digital";
  readonly channel: number;
  readonly label: string;
  readonly lane: number;
  readonly segments: readonly DigitalSegment[];
}

export type Trace = AnalogTrace | AnalogEnvelopeTrace | DigitalTrace;

export interface TraceSet {
  readonly traces: readonly Trace[];
}

/** One analog panel: a channel drawn in volts, scaled per the capture's range. */
export interface AnalogPanelSpec {
  readonly kind: "analog";
  readonly channel: number;
  readonly label: string;
  /** The selected vertical range, volts full-scale (SPEC §5.2 "volts/div"). */
  readonly rangeV: number;
  readonly offsetV: number;
}

/** One digital panel: a channel drawn as a stacked lane. */
export interface DigitalPanelSpec {
  readonly kind: "digital";
  readonly channel: number;
  readonly label: string;
  readonly lane: number;
}

export type PanelSpec = AnalogPanelSpec | DigitalPanelSpec;

/** Static plot structure — which panels exist and in what order. Traces (the data) are separate. */
export interface Layout {
  readonly panels: readonly PanelSpec[];
}

/**
 * One decoded annotation, positioned in time (SPEC §6, §11.3's "annotation
 * rendering under the traces"). `level` nests frame (0) -> field (1) -> bit
 * (2) by convention, not containment the renderer has to compute — a
 * decoder already emits it that way.
 */
export interface AnnotationBox {
  readonly t0: number;
  readonly t1: number;
  readonly level: number;
  readonly label: string;
}

/** One digital channel's decoded annotations, drawn under that channel's lane. */
export interface AnnotationLane {
  readonly channel: number;
  readonly boxes: readonly AnnotationBox[];
}

export type RendererId = "plotly" | "webgl2" | "uplot";

/**
 * The handful of theme colors a renderer needs to keep a plot legible in
 * both themes (SPEC §11.1) — read straight off AWIC's own CSS custom
 * properties (`render/plotly-theme.ts`) rather than duplicated as a second
 * color table, so light/dark/system all stay in sync with the one place
 * that defines them (`index.html`).
 */
export interface ThemeColors {
  readonly background: string;
  readonly foreground: string;
  readonly mutedForeground: string;
  readonly border: string;
}

/**
 * SPECIFICATION.md §5.3. One interface, switchable at runtime, so the graph
 * module never depends on a specific charting library. `webgl2` is the
 * default (D-05) and `plotly` is fully implemented too; `uplot` is a
 * registered seam that reports its own not-yet-implemented state rather
 * than half-rendering (see `render/uplot.ts`).
 */
export interface WaveformRenderer {
  readonly id: RendererId;
  mount(el: HTMLElement, layout: Layout): void;
  /**
   * Replaces the panel layout of an already-mounted plot: the lanes' ranges,
   * offsets or membership changed, the data did not. Re-`mount()`ing for that
   * costs a fresh WebGL context (or a full Plotly `newPlot`) on every
   * keystroke in a range or offset field (B-55); a backend that was never
   * successfully mounted may treat it as a no-op.
   */
  setLayout(layout: Layout): void;
  setTraces(traces: TraceSet): void;
  /**
   * Decoded protocol annotations to draw under each digital channel's lane
   * (SPEC §6, T21). Optional to implement meaningfully: a backend that
   * doesn't support annotation lanes (`plotly`, `uplot`) still exposes the
   * method — a no-op — so the graph module never needs to check which
   * renderer is mounted before calling it. An empty array clears whatever
   * was drawn before.
   */
  setAnnotations(lanes: readonly AnnotationLane[]): void;
  setViewport(t0: number, t1: number): void;
  /**
   * Subscribes to viewport changes driven by user interaction with the
   * mounted plot (wheel-zoom, drag-pan, double-click-fit) — never fired for
   * a `setViewport()` call the caller itself made. Live mode (§5.5) turns
   * this into `rec.view.update`; today nothing downstream depends on it, but
   * every implemented backend still reports it faithfully rather than
   * leaving it a silent no-op. Returns an unsubscribe function.
   */
  onViewportChange(cb: (t0: number, t1: number) => void): () => void;
  /** Applies (or re-applies, on a theme switch) the current theme colors. A no-op for a backend never successfully mounted. */
  setTheme(colors: ThemeColors): void;
  destroy(): void;
}

/** Thrown by a registered-but-unimplemented backend's `mount()`. */
export class RendererNotImplementedError extends Error {
  constructor(readonly rendererId: RendererId) {
    super(`renderer "${rendererId}" is registered but not yet implemented`);
    this.name = "RendererNotImplementedError";
  }
}

/**
 * Thrown by `webgl2`'s `mount()` when `canvas.getContext("webgl2")` returns
 * `null` — an old browser, or WebGL disabled by policy or `--disable-gpu`.
 * Distinct from `RendererNotImplementedError`: the backend exists and works,
 * this specific browser just can't run it, so the graph module's fallback
 * (SPEC §5.3 task instructions: "fall back to Plotly with a visible note
 * rather than a blank panel") is a runtime environment check, not a
 * "pick another renderer" prompt.
 */
export class RendererUnavailableError extends Error {
  constructor(readonly rendererId: RendererId) {
    super(`renderer "${rendererId}" is unavailable in this browser`);
    this.name = "RendererUnavailableError";
  }
}
