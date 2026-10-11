import type { AnalogEnvelopeTrace, AnalogTrace, DigitalTrace } from "./types";

/**
 * Pure vertex-buffer construction for the WebGL2 renderer (SPEC §5.3). Split
 * out of `webgl2.ts` so the geometry math — which trace kind becomes which
 * primitive, and the time-precision handling — is unit-testable without a
 * `<canvas>` or a GL context (this project's vitest runs plain Node).
 *
 * Every builder takes `origin` (seconds) and subtracts it from every time
 * value *before* narrowing to `Float32Array`: a capture-relative time can be
 * anything (SPEC's live/continuous recordings run to hours), and float32 has
 * ~7 significant decimal digits, so a raw absolute time of, say, 1000 s
 * zoomed to a 1 µs span would need 10 significant digits and jitter visibly.
 * `webgl2.ts` picks `origin` once per `setTraces()` call (typically the
 * trace set's own start) so every local value here stays close to zero,
 * where float32 has plenty of precision to spare — the GPU-side transform
 * then only ever adds back a *small* delta (SPEC: "the viewport origin"),
 * computed fresh each frame in JS double precision.
 */

/** Interleaved `(x, y)` float32 pairs, ready to upload as a `vertexAttribPointer` with `size: 2`. */
export type Vertices = Float32Array;

/** One analog channel, raw: a single line strip (`gl.LINE_STRIP`). */
export function buildAnalogLineVertices(trace: AnalogTrace, origin: number): Vertices {
  const n = trace.t.length;
  const out = new Float32Array(n * 2);
  for (let i = 0; i < n; i++) {
    out[i * 2] = trace.t[i]! - origin;
    out[i * 2 + 1] = trace.v[i]!;
  }
  return out;
}

/**
 * One analog channel's min/max envelope: a filled band (SPEC §3.2/§5.3 —
 * "never collapsed to a single value"). Zoomed out, a bucket can be a
 * fraction of a pixel wide — a flat, lightly noisy channel especially, since
 * its envelope height barely varies from one bucket to the next — and a
 * *fan* of independent, sub-pixel-wide triangle pairs rasterizes
 * inconsistently at that scale: each bucket's quad is its own primitive, so
 * the GPU is free to round it on or off a sample independently of its
 * neighbours, which is what "renders as dotted dashes" looks
 * like. One continuous `gl.TRIANGLE_STRIP` across every bucket removes that
 * degree of freedom: consecutive buckets share their boundary vertices
 * exactly (`t1[i]` and `t0[i+1]` are the same instant, SPEC §3.4's "item
 * boundaries are aligned"), so the strip is one unbroken shape with no
 * internal edge for the rasterizer to treat inconsistently. Where the two
 * buckets' heights differ, the shared-x vertices form a zero-area "riser"
 * triangle pair — contributing no stray pixels, just the vertical step a
 * min/max histogram is supposed to show.
 */
export function buildAnalogEnvelopeVertices(trace: AnalogEnvelopeTrace, origin: number): Vertices {
  const n = trace.t0.length;
  const out = new Float32Array(n * 4 * 2);
  let o = 0;
  for (let i = 0; i < n; i++) {
    const x0 = trace.t0[i]! - origin;
    const x1 = trace.t1[i]! - origin;
    const yMin = trace.vMin[i]!;
    const yMax = trace.vMax[i]!;
    out[o++] = x0; out[o++] = yMin;
    out[o++] = x0; out[o++] = yMax;
    out[o++] = x1; out[o++] = yMin;
    out[o++] = x1; out[o++] = yMax;
  }
  return out;
}

/**
 * The envelope band's outline ("a continuous filled band with
 * an outline"): the stepped max profile and the stepped min profile, each a
 * `gl.LINE_STRIP`. Unlike the semi-transparent fill, a line strip is drawn
 * as one connected stroke regardless of how thin an individual bucket is —
 * it cannot itself look "dotted" — so it gives the band a crisp, always-
 * continuous edge even in the degenerate case the fill alone is fragile to.
 */
export interface EnvelopeOutlineVertices {
  readonly top: Vertices;
  readonly bottom: Vertices;
}

export function buildAnalogEnvelopeOutlineVertices(trace: AnalogEnvelopeTrace, origin: number): EnvelopeOutlineVertices {
  const n = trace.t0.length;
  const top = new Float32Array(n * 2 * 2);
  const bottom = new Float32Array(n * 2 * 2);
  let ot = 0;
  let ob = 0;
  for (let i = 0; i < n; i++) {
    const x0 = trace.t0[i]! - origin;
    const x1 = trace.t1[i]! - origin;
    const yMin = trace.vMin[i]!;
    const yMax = trace.vMax[i]!;
    top[ot++] = x0; top[ot++] = yMax;
    top[ot++] = x1; top[ot++] = yMax;
    bottom[ob++] = x0; bottom[ob++] = yMin;
    bottom[ob++] = x1; bottom[ob++] = yMin;
  }
  return { top, bottom };
}

/** `0` (low) or `1` (high) in this trace's own vertical unit; panel placement is a draw-time transform. */
const LOW = 0;
const HIGH = 1;

/**
 * One digital channel's level line (SPEC §5.3: "level lines with vertical
 * edges"), as independent `gl.LINES` segments rather than one `LINE_STRIP` —
 * a `"toggled"` bucket (SPEC §3.2) must never be drawn as a level, so the
 * line has to *break* there, and `LINES` (each pair a separate segment)
 * breaks for free where a strip would need index tricks (WebGL2 has no
 * portable primitive-restart for `LINE_STRIP`).
 */
export function buildDigitalLevelVertices(trace: DigitalTrace, origin: number): Vertices {
  const points: number[] = [];
  let prevEnd: { x: number; y: number } | null = null;
  for (const seg of trace.segments) {
    if (seg.state === "toggled") {
      prevEnd = null; // the level is genuinely unknown here — break the line
      continue;
    }
    const y = seg.state === "high" ? HIGH : LOW;
    const x0 = seg.t0 - origin;
    const x1 = seg.t1 - origin;
    // Vertical edge from the previous segment's level to this one's, only when contiguous.
    if (prevEnd && prevEnd.x === x0 && prevEnd.y !== y) {
      points.push(x0, prevEnd.y, x0, y);
    }
    points.push(x0, y, x1, y);
    prevEnd = { x: x1, y };
  }
  return new Float32Array(points);
}

/**
 * One digital channel's `"toggled"` spans as a filled band across the full
 * lane height (SPEC §3.2/§5.3: never a level). Unlike the analog envelope, a
 * toggled bucket's height never varies (always the full `LOW..HIGH` lane),
 * so a run of *contiguous* toggled buckets is merged into a single quad
 * rather than one independent quad per bucket — the same "adjacent buckets
 * form one continuous band, no gaps" fix as the analog envelope, and stronger here: there's no per-bucket
 * height information to lose
 * by merging, so a run collapses to one primitive instead of just sharing
 * vertices between many. A run only breaks where the data itself breaks
 * (a non-toggled segment intervenes), matching how [`buildDigitalLevelVertices`]
 * already breaks the level line at a toggled segment.
 */
export function buildDigitalToggleVertices(trace: DigitalTrace, origin: number): Vertices {
  const runs: Array<{ t0: number; t1: number }> = [];
  for (const seg of trace.segments) {
    if (seg.state !== "toggled") continue;
    const last = runs[runs.length - 1];
    if (last && last.t1 === seg.t0) {
      last.t1 = seg.t1; // contiguous with the run in progress — extend it
    } else {
      runs.push({ t0: seg.t0, t1: seg.t1 });
    }
  }
  const out = new Float32Array(runs.length * 6 * 2);
  let o = 0;
  for (const run of runs) {
    const x0 = run.t0 - origin;
    const x1 = run.t1 - origin;
    out[o++] = x0; out[o++] = LOW;
    out[o++] = x1; out[o++] = LOW;
    out[o++] = x1; out[o++] = HIGH;
    out[o++] = x0; out[o++] = LOW;
    out[o++] = x1; out[o++] = HIGH;
    out[o++] = x0; out[o++] = HIGH;
  }
  return out;
}
