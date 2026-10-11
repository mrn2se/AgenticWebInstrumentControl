/**
 * Pure clip-space transform math for the WebGL2 renderer (SPEC §5.3): where
 * panel *i* of *n* sits vertically in a stacked-lane layout, and the
 * linear scale/offset that maps a value range (volts, or the digital 0..1
 * lane unit) onto that panel's row. No DOM, no GL — the vertex shader is
 * handed `scale`/`offset` as uniforms and does `clip = value * scale +
 * offset`, so this is exactly what it needs to compute, and exactly what a
 * test can check without a GL context.
 */
export interface AxisTransform {
  readonly scale: number;
  readonly offset: number;
}

export interface ClipRow {
  /** Clip-space Y at the top of the row (closer to +1). */
  readonly top: number;
  /** Clip-space Y at the bottom of the row (closer to -1). */
  readonly bottom: number;
}

/** Space left between stacked panel rows, as a fraction of one row's height. */
const PANEL_GAP_FRACTION = 0.06;

/** Clip-space `[top, bottom]` for panel `index` of `count`, stacked top-to-bottom with a small gap between rows. */
export function panelRowClip(index: number, count: number): ClipRow {
  const n = Math.max(count, 1);
  const rowHeight = 2 / n;
  const gap = rowHeight * PANEL_GAP_FRACTION;
  return {
    top: 1 - index * rowHeight - gap / 2,
    bottom: 1 - (index + 1) * rowHeight + gap / 2,
  };
}

/**
 * Maps the value range `[offset - range/2, offset + range/2]` linearly onto
 * clip-space `[bottom, top]`. `range <= 0` (a misconfigured panel) maps
 * everything to the row's vertical center rather than dividing by zero.
 */
export function valueAxisTransform(top: number, bottom: number, range: number, offset: number): AxisTransform {
  if (!(range > 0)) return { scale: 0, offset: (top + bottom) / 2 };
  const scale = (top - bottom) / range;
  return { scale, offset: bottom - (offset - range / 2) * scale };
}

/**
 * Maps time (already shifted by `originSeconds`, SPEC precision note — see
 * `webgl2-geometry.ts`) onto clip-space `[-1, 1]` for the viewport
 * `[t0, t1]`. Recomputed every frame from the current viewport in JS double
 * precision; only the small result crosses into float32 as a uniform.
 */
export function timeAxisTransform(t0: number, t1: number, originSeconds: number): AxisTransform {
  const span = t1 - t0;
  if (!(span > 0)) return { scale: 0, offset: 0 };
  const scale = 2 / span;
  return { scale, offset: -1 - (t0 - originSeconds) * scale };
}

/** The digital lane's fixed value range (0 = low, 1 = high) with a small margin, matching `plotly-mapping.ts`'s `digitalAxisRange`. */
export const DIGITAL_RANGE = 1.3;
export const DIGITAL_OFFSET = 0.5;

/**
 * The volts a panel's lane covers: `[offsetV - rangeV, offsetV + rangeV]`.
 *
 * `rangeV` is the device's own range setting, and SPEC §5.2 says what that
 * means — "`AcqConfig.ranges_v` carries the wanted **± full-scale** range per
 * enabled channel". The oscilloscope's 5 V range returns code 0 as -5 V and code 255
 * as +5 V, as its calibration reports. Treating it as the total span made
 * every analog lane half as tall as the hardware it was showing, so a CAN pair
 * sitting at 2.7-4.1 V on the 5 V range fell outside a lane drawn to +2.5 V
 * (B-55).
 */
export function analogPanelBounds(rangeV: number, offsetV: number): { readonly min: number; readonly max: number } {
  const half = Math.abs(rangeV);
  return { min: offsetV - half, max: offsetV + half };
}

/** The total height of an analog lane in volts - twice its ± range. */
export function analogPanelSpan(rangeV: number): number {
  return 2 * Math.abs(rangeV);
}

/**
 * The device-pixel scissor box for panel `index` of `count`, so a trace that
 * leaves its own lane is not drawn over its neighbours.
 *
 * GL clips at the edge of the drawing buffer, not at the edge of a row: a
 * channel whose signal sat above its lane was drawn straight through the lane
 * above it, which is how CH2 came to appear inside CH1's lane (B-55). Rows are
 * in clip space, `+1` at the top; scissor boxes are in pixels from the
 * *bottom*, which is the one conversion this needs to get right.
 */
export function panelScissorRect(
  index: number,
  count: number,
  pxW: number,
  pxH: number,
): { readonly x: number; readonly y: number; readonly width: number; readonly height: number } {
  const row = panelRowClip(index, count);
  const yTop = ((1 - row.top) / 2) * pxH;
  const yBottom = ((1 - row.bottom) / 2) * pxH;
  const height = Math.max(0, Math.round(yBottom - yTop));
  return { x: 0, y: Math.max(0, Math.round(pxH - yBottom)), width: Math.max(0, Math.round(pxW)), height };
}
