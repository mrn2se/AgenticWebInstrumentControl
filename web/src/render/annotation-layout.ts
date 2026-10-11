/**
 * Pure geometry for the annotation lanes a `WaveformRenderer` draws under a
 * digital trace (SPECIFICATION.md §6: "so the UI can stack them under the
 * waveform"; T21: "frames as boxes with text labels ... fields and bits as
 * finer boxes when zoomed in. Levels stack. Hide labels that don't fit.").
 *
 * Kept free of any canvas/DOM API (no `measureText`) so it is unit-testable
 * without a browser context, exactly like `webgl2-geometry.ts`/
 * `webgl2-viewport.ts` — `render/webgl2.ts` is the only caller, and it is the
 * one place a canvas actually exists.
 */

/** One decoded annotation, already in the time domain (seconds). */
export interface AnnotationBoxInput {
  readonly t0: number;
  readonly t1: number;
  /** 0 = frame, 1 = field, 2 = bit (SPEC §6). */
  readonly level: number;
  readonly label: string;
}

/** One box positioned in pixel space for a specific viewport/width. */
export interface LaidOutAnnotationBox {
  readonly x0: number;
  readonly x1: number;
  readonly level: number;
  readonly label: string;
  /** `false` when the estimated label width exceeds the box — draw the box, not the text. */
  readonly showLabel: boolean;
}

export interface AnnotationLayoutOptions {
  readonly widthPx: number;
  readonly viewport: { readonly t0: number; readonly t1: number };
  /** A box narrower than this never shows a label, however short (default 10px: an unreadable sliver either way). */
  readonly minLabelWidthPx?: number;
  /** Estimated px/character for the label-fit heuristic — deliberately not a real `measureText` (see module doc). */
  readonly charWidthPx?: number;
}

const DEFAULT_MIN_LABEL_WIDTH_PX = 10;
const DEFAULT_CHAR_WIDTH_PX = 6;

/**
 * Places every box in `[0, widthPx]` pixel space for the given viewport,
 * dropping boxes entirely outside it and clipping ones that straddle an
 * edge. Boxes keep their input order (which is also draw order); stacking by
 * level is [`levelRowBand`]'s job, kept separate because it only needs a
 * level number, not the rest of a box's shape.
 */
export function layoutAnnotations(
  boxes: readonly AnnotationBoxInput[],
  opts: AnnotationLayoutOptions,
): LaidOutAnnotationBox[] {
  const { widthPx, viewport } = opts;
  const span = viewport.t1 - viewport.t0;
  if (!(span > 0) || !(widthPx > 0)) return [];
  const minLabelWidth = opts.minLabelWidthPx ?? DEFAULT_MIN_LABEL_WIDTH_PX;
  const charWidth = opts.charWidthPx ?? DEFAULT_CHAR_WIDTH_PX;

  const out: LaidOutAnnotationBox[] = [];
  for (const box of boxes) {
    if (box.t1 <= viewport.t0 || box.t0 >= viewport.t1) continue;
    const rawX0 = ((box.t0 - viewport.t0) / span) * widthPx;
    const rawX1 = ((box.t1 - viewport.t0) / span) * widthPx;
    const x0 = Math.max(0, rawX0);
    const x1 = Math.min(widthPx, rawX1);
    if (x1 <= x0) continue;
    const boxWidth = x1 - x0;
    // A point-in-time annotation (I2C's START/STOP/ACK, SPEC's own examples)
    // has `t0 === t1`: draw it as a thin marker rather than dropping it, but
    // it can never fit a label — there is no box to fit one into.
    const labelWidth = box.label.length * charWidth;
    const showLabel = boxWidth >= minLabelWidth && labelWidth <= boxWidth;
    out.push({ x0, x1, level: box.level, label: box.label, showLabel });
  }
  return out;
}

/**
 * The vertical pixel band one level occupies within a lane's own allotted
 * height, levels stacked top-to-bottom in level order (0 nearest the trace).
 * `levelCount` is a fixed 3 by default (SPEC §6's frame/field/bit) rather
 * than derived from what's actually present, so a lane's row heights don't
 * jump around as different decode ranges happen to include or omit a level.
 */
export function levelRowBand(
  level: number,
  laneTopPx: number,
  laneBottomPx: number,
  levelCount = 3,
): { readonly top: number; readonly bottom: number } {
  const laneHeight = Math.max(0, laneBottomPx - laneTopPx);
  const rows = Math.max(1, levelCount);
  const rowHeight = laneHeight / rows;
  const clampedLevel = Math.min(Math.max(level, 0), rows - 1);
  const top = laneTopPx + clampedLevel * rowHeight;
  return { top, bottom: top + rowHeight };
}
