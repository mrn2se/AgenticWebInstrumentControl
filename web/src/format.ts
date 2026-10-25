/**
 * SI-unit formatting for a value shown as standalone text next to a label —
 * a pattern-match timestamp, a wait result — as opposed to an axis tick
 * label, which picks its unit from the shared tick spacing
 * (`render/webgl2-axis.ts`'s `formatSiTime`) so every tick on one axis
 * agrees. Here there is no shared spacing to borrow, so the unit comes from
 * the value's own magnitude instead.
 */

interface SiUnit {
  readonly exponent: number;
  readonly suffix: string;
}

/** Same ladder as the axis formatter (SPEC §5.3: "time in SI units (ns/µs/ms/s)"). */
const TIME_UNITS: readonly SiUnit[] = [
  { exponent: 0, suffix: "s" },
  { exponent: -3, suffix: "ms" },
  { exponent: -6, suffix: "µs" },
  { exponent: -9, suffix: "ns" },
];

function pickTimeUnit(magnitude: number): SiUnit {
  if (!(magnitude > 0)) return TIME_UNITS[0]!;
  for (const unit of TIME_UNITS) {
    if (magnitude >= 10 ** unit.exponent) return unit;
  }
  return TIME_UNITS[TIME_UNITS.length - 1]!;
}

/**
 * Formats a duration or instant in seconds as e.g. `"1.952 ms"` rather than
 * `"0.0019 s"`: the unit is chosen from `seconds`'s own
 * magnitude, then a fixed 3 decimals are kept within that unit. Three
 * decimals in the "s" unit is millisecond resolution — enough to tell
 * adjacent 1 kHz bit-pattern edges (1 ms apart, SPEC §2.9.2) apart even at
 * that scale — and finer still once the value is small enough to report in
 * "ms" or "µs".
 */
export function formatSiDuration(seconds: number): string {
  const unit = pickTimeUnit(Math.abs(seconds));
  const divisor = 10 ** unit.exponent;
  return `${(seconds / divisor).toFixed(3)} ${unit.suffix}`;
}

/** Same ladder, upward, for frequencies. */
const FREQUENCY_UNITS: readonly SiUnit[] = [
  { exponent: 9, suffix: "GHz" },
  { exponent: 6, suffix: "MHz" },
  { exponent: 3, suffix: "kHz" },
  { exponent: 0, suffix: "Hz" },
];

function pickFrequencyUnit(magnitude: number): SiUnit {
  if (!(magnitude > 0)) return FREQUENCY_UNITS[FREQUENCY_UNITS.length - 1]!;
  for (const unit of FREQUENCY_UNITS) {
    if (magnitude >= 10 ** unit.exponent) return unit;
  }
  return FREQUENCY_UNITS[FREQUENCY_UNITS.length - 1]!;
}

/**
 * Formats a frequency in Hz as e.g. `"1.000 kHz"` rather than
 * `Number.toPrecision`'s exponential notation (`"1.000e+6"`) once the value
 * clears 4 significant digits — the same "picks a unit, not just a decimal
 * count" fix as [`formatSiDuration`], for the graph's cursor `1/Δt` readout.
 */
export function formatSiFrequency(hz: number): string {
  const unit = pickFrequencyUnit(Math.abs(hz));
  const divisor = 10 ** unit.exponent;
  return `${(hz / divisor).toFixed(3)} ${unit.suffix}`;
}

/** SI prefixes for a value with any base unit, pico to giga. */
const SI_PREFIXES: readonly SiUnit[] = [
  { exponent: 9, suffix: "G" },
  { exponent: 6, suffix: "M" },
  { exponent: 3, suffix: "k" },
  { exponent: 0, suffix: "" },
  { exponent: -3, suffix: "m" },
  { exponent: -6, suffix: "µ" },
  { exponent: -9, suffix: "n" },
  { exponent: -12, suffix: "p" },
];

/**
 * `value` with an SI prefix on `unit` and `significant` digits, e.g.
 * `formatSi(8.18705e-7, "s", 3)` is `"819 ns"` and `formatSi(0.001, "s", 4)`
 * is `"1.000 ms"`. Rounding happens before the prefix is chosen, so 999.96 µs
 * at 4 digits reads `"1.000 ms"`, not `"1000 µs"`.
 */
export function formatSi(value: number, unit: string, significant = 4): string {
  if (!Number.isFinite(value)) return `${value} ${unit}`.trim();
  if (value === 0) return `${(0).toFixed(Math.max(0, significant - 1))} ${unit}`.trim();
  const rounded = Number(value.toPrecision(significant));
  const magnitude = Math.abs(rounded);
  const prefix =
    SI_PREFIXES.find((p) => magnitude >= 10 ** p.exponent) ?? SI_PREFIXES[SI_PREFIXES.length - 1]!;
  const scaled = rounded / 10 ** prefix.exponent;
  const integerDigits = Math.max(1, Math.floor(Math.log10(Math.abs(scaled))) + 1);
  const decimals = Math.max(0, significant - integerDigits);
  return `${scaled.toFixed(decimals)} ${prefix.suffix}${unit}`;
}

/**
 * A sample rate as a scope labels it, `"16 MS/s"` or `"62.5 kS/s"`: samples
 * per second are not a frequency, and `"16.000 MHz"` read like one. Trailing
 * zeros are dropped — a rate ladder lists round numbers.
 */
export function formatSampleRate(hz: number): string {
  const unit = pickFrequencyUnit(Math.abs(hz));
  const scaled = hz / 10 ** unit.exponent;
  const text = String(Number(scaled.toFixed(3)));
  return `${text} ${unit.suffix.replace("Hz", "S/s")}`;
}
