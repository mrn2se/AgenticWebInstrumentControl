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
