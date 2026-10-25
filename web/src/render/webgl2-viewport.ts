import type { Trace } from "./types";

/**
 * Pure viewport and pointer-mapping math for the WebGL2 renderer (SPEC
 * §5.3): wheel-zoom-about-pointer, drag-pan, fit-to-data, and the
 * pointer<->time conversion the interaction handlers need. No DOM, no GL —
 * testable in plain Node (`vite.config.ts` runs vitest with `environment:
 * "node"`), which is the point of splitting it out of `webgl2.ts`.
 */
export interface Viewport {
  readonly t0: number;
  readonly t1: number;
}

/** A zoom is never allowed to collapse the viewport to (or past) zero width. */
const MIN_SPAN_SECONDS = 1e-12;

/** Zooms `viewport` by `factor` (>1 zooms out, <1 zooms in) about `pivot`, which stays fixed on screen. */
export function zoomViewport(viewport: Viewport, pivot: number, factor: number): Viewport {
  const span = viewport.t1 - viewport.t0;
  if (!(span > 0) || !(factor > 0)) return viewport;
  const newSpan = Math.max(span * factor, MIN_SPAN_SECONDS);
  const ratio = (pivot - viewport.t0) / span;
  const t0 = pivot - ratio * newSpan;
  return { t0, t1: t0 + newSpan };
}

/** Pans `viewport` by a fixed offset in seconds; the span is unchanged. */
export function panViewport(viewport: Viewport, deltaSeconds: number): Viewport {
  return { t0: viewport.t0 + deltaSeconds, t1: viewport.t1 + deltaSeconds };
}

/** Maps a canvas-relative pointer x (CSS pixels, origin at the canvas's left edge) to a time. */
export function pointerXToTime(xCss: number, widthCss: number, viewport: Viewport): number {
  if (!(widthCss > 0)) return viewport.t0;
  const frac = xCss / widthCss;
  return viewport.t0 + frac * (viewport.t1 - viewport.t0);
}

/** The inverse of `pointerXToTime`: where a time `t` falls on the canvas, in CSS pixels. */
export function timeToPointerX(t: number, widthCss: number, viewport: Viewport): number {
  const span = viewport.t1 - viewport.t0;
  if (!(span > 0)) return 0;
  return ((t - viewport.t0) / span) * widthCss;
}

/** The full time extent covered by a trace set. `null` for an empty set, so callers never "fit" to a degenerate range. */
export function traceSetTimeBounds(traces: readonly Trace[]): Viewport | null {
  let min = Infinity;
  let max = -Infinity;
  for (const trace of traces) {
    switch (trace.kind) {
      case "analog":
        for (const t of trace.t) {
          if (t < min) min = t;
          if (t > max) max = t;
        }
        break;
      case "analog-envelope":
        for (const t of trace.t0) if (t < min) min = t;
        for (const t of trace.t1) if (t > max) max = t;
        break;
      case "digital":
        for (const seg of trace.segments) {
          if (seg.t0 < min) min = seg.t0;
          if (seg.t1 > max) max = seg.t1;
        }
        break;
    }
  }
  if (!Number.isFinite(min) || !Number.isFinite(max) || max <= min) return null;
  return { t0: min, t1: max };
}

/** Fits a viewport to `bounds` with a small margin so traces don't touch the panel edges. */
export function fitAllViewport(bounds: Viewport, paddingFraction = 0.02): Viewport {
  const span = bounds.t1 - bounds.t0;
  const pad = span > 0 ? span * paddingFraction : 1;
  return { t0: bounds.t0 - pad, t1: bounds.t1 + pad };
}

/**
 * Keeps a user zoom or pan inside the data: the span never grows past
 * `extent`'s fit-all view, and the window slides back so that at least half
 * of it shows data. The margin allowed past either edge is the fit-all
 * padding or half the visible span, whichever is smaller — a fixed margin
 * let a deeply zoomed view rest entirely in the padding, showing nothing.
 * `extent` is the data's own time range; a `null` extent (nothing drawn
 * yet) leaves the viewport as it is.
 */
export function clampViewport(viewport: Viewport, extent: Viewport | null): Viewport {
  if (!extent) return viewport;
  const fit = fitAllViewport(extent);
  const maxSpan = fit.t1 - fit.t0;
  const span = Math.min(viewport.t1 - viewport.t0, maxSpan);
  if (!(span > 0)) return viewport;
  const margin = Math.min(extent.t0 - fit.t0, span / 2);
  const lo = extent.t0 - margin;
  const hi = extent.t1 + margin;
  let t0 = viewport.t0;
  if (t0 < lo) t0 = lo;
  if (t0 + span > hi) t0 = hi - span;
  return { t0, t1: t0 + span };
}

/** CSS pixels per wheel "line" (`deltaMode === 1`), about one line of text — what browsers scroll a page by per notch. */
const WHEEL_LINE_PX = 16;

/**
 * A wheel delta in CSS pixels whatever unit the browser reported it in:
 * pixels (`deltaMode` 0, most trackpads and Chrome), lines (1, Firefox on
 * Linux with many mice, about 3 per notch) or pages (2). Without this a
 * line-mode notch moved the view by a fraction of a percent.
 */
export function wheelDeltaPixels(delta: number, deltaMode: number, pagePx: number): number {
  if (deltaMode === 1) return delta * WHEEL_LINE_PX;
  if (deltaMode === 2) return delta * Math.max(pagePx, 1);
  return delta;
}

export interface WheelInput {
  /** Both already in CSS pixels (`wheelDeltaPixels`). */
  readonly dx: number;
  readonly dy: number;
  readonly shiftKey: boolean;
  /** Set by browsers for a trackpad pinch as well as Ctrl+wheel. */
  readonly ctrlKey: boolean;
}

export type WheelAction =
  | { readonly kind: "zoom"; readonly factor: number }
  | { readonly kind: "pan"; readonly px: number }
  | { readonly kind: "none" };

/**
 * What one wheel event does to the time axis. Pinch (Ctrl) and a plain
 * vertical wheel zoom about the pointer; a horizontal swipe, or Shift+wheel,
 * pans. A trackpad swipe reports both axes at once, so the larger one
 * decides — zooming and panning on the same diagonal gesture is unreadable.
 */
export function wheelAction(input: WheelInput): WheelAction {
  const { dx, dy } = input;
  if (input.ctrlKey) {
    // A pinch reports small deltas many times a second; it needs a steeper
    // response than a wheel notch to feel the same.
    return dy === 0 ? { kind: "none" } : { kind: "zoom", factor: Math.exp(dy * 0.01) };
  }
  if (input.shiftKey) {
    // Some browsers already turn Shift+wheel into a horizontal delta.
    const px = dx !== 0 ? dx : dy;
    return px === 0 ? { kind: "none" } : { kind: "pan", px };
  }
  if (Math.abs(dx) > Math.abs(dy)) return { kind: "pan", px: dx };
  // dy > 0 (scroll down) zooms out; a smooth exponential response feels
  // closer to a real scope's knob than a fixed step.
  return dy === 0 ? { kind: "none" } : { kind: "zoom", factor: Math.exp(dy * 0.002) };
}
