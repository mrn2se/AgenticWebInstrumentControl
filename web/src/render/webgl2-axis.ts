/**
 * Pure axis-tick generation and SI-unit formatting for the WebGL2 renderer's
 * 2D canvas overlay (SPEC §5.3: "time in SI units (ns/µs/ms/s), volts per
 * analog panel"). No DOM — the overlay drawing in `webgl2.ts` just asks this
 * module for tick positions and label text.
 */

/** Rounds `roughStep` up to a "nice" 1/2/5 × 10^n step, the standard axis-tick heuristic. */
export function niceStep(roughStep: number): number {
  if (!(roughStep > 0)) return 0;
  const exponent = Math.floor(Math.log10(roughStep));
  const base = 10 ** exponent;
  const fraction = roughStep / base;
  const niceFraction = fraction <= 1 ? 1 : fraction <= 2 ? 2 : fraction <= 5 ? 5 : 10;
  return niceFraction * base;
}

/** Tick positions spanning `[t0, t1]`, spaced at a "nice" step targeting roughly `targetCount` ticks. */
export function timeTicks(t0: number, t1: number, targetCount = 6): number[] {
  if (!(t1 > t0) || targetCount < 1) return [];
  const step = niceStep((t1 - t0) / targetCount);
  if (!(step > 0)) return [];
  const firstIndex = Math.ceil(t0 / step);
  const lastIndex = Math.floor(t1 / step);
  const ticks: number[] = [];
  for (let i = firstIndex; i <= lastIndex; i++) ticks.push(i * step);
  return ticks;
}

interface SiUnit {
  readonly exponent: number;
  readonly suffix: string;
}

/** SI prefixes AWIC's time axes ever need, s down to ns (SPEC §5.3), each a bucket 1000x wide. */
const TIME_UNITS: readonly SiUnit[] = [
  { exponent: 0, suffix: "s" },
  { exponent: -3, suffix: "ms" },
  { exponent: -6, suffix: "µs" },
  { exponent: -9, suffix: "ns" },
];
const MIN_EXPONENT = TIME_UNITS[TIME_UNITS.length - 1]!.exponent;
const MAX_EXPONENT = TIME_UNITS[0]!.exponent;

/**
 * Picks the SI unit whose scale is *nearest* to `magnitude` (rounding to the
 * closest power of 1000, clamped to the s..ns range), rather than the
 * largest unit `magnitude` merely clears — a step of 0.5 ms should read
 * "0.5 ms", not get pushed down to "500 µs" just because it's under 1 ms.
 */
function pickTimeUnit(magnitude: number): SiUnit {
  if (!(magnitude > 0)) return TIME_UNITS[TIME_UNITS.length - 1]!;
  const bucket = Math.max(MIN_EXPONENT, Math.min(MAX_EXPONENT, Math.round(Math.log10(magnitude) / 3) * 3));
  return TIME_UNITS.find((u) => u.exponent === bucket) ?? TIME_UNITS[TIME_UNITS.length - 1]!;
}

/**
 * Formats `t` (seconds) using the SI unit appropriate for `stepSeconds` — the
 * *tick spacing*, not `t` itself, decides the unit, so every label on one
 * axis shares a unit even when a tick value happens to be exactly 0.
 */
export function formatSiTime(t: number, stepSeconds: number): string {
  const magnitude = Math.abs(stepSeconds);
  const unit = pickTimeUnit(magnitude);
  const divisor = 10 ** unit.exponent;
  const scaledStep = magnitude / divisor;
  const decimals = scaledStep > 0 ? Math.max(0, Math.min(6, -Math.floor(Math.log10(scaledStep)))) : 0;
  return `${(t / divisor).toFixed(decimals)} ${unit.suffix}`;
}

/** Tick positions spanning `[vMin, vMax]`, spaced at a "nice" step targeting roughly `targetCount` ticks. */
export function voltTicks(vMin: number, vMax: number, targetCount = 4): number[] {
  if (!(vMax > vMin) || targetCount < 1) return [];
  const step = niceStep((vMax - vMin) / targetCount);
  if (!(step > 0)) return [];
  const firstIndex = Math.ceil(vMin / step);
  const lastIndex = Math.floor(vMax / step);
  const ticks: number[] = [];
  for (let i = firstIndex; i <= lastIndex; i++) ticks.push(i * step);
  return ticks;
}

/** Formats a volts tick, with decimal precision derived from the tick spacing. */
export function formatVolts(v: number, stepVolts: number): string {
  const magnitude = Math.abs(stepVolts) || Math.abs(v) || 1;
  const decimals = Math.max(0, Math.min(6, -Math.floor(Math.log10(magnitude))));
  return `${v.toFixed(decimals)} V`;
}
