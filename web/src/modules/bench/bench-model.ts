import type { SimStatus } from "../../proto/types";

/** One row of the test-point table: what it is, and what is clipped to it. */
export interface TestPointRow {
  readonly id: string;
  readonly name: string;
  readonly description: string;
  readonly kind: string;
  /** `"Logic #1 D3"` for every probe known to be on it. */
  readonly probes: readonly string[];
}

/** A short name for an instrument, from its label. */
export function instrumentShortName(label: string): string {
  if (/#2/.test(label)) return "Logic #2";
  if (/logic/i.test(label)) return "Logic #1";
  if (/scope|oscillo/i.test(label)) return "Scope";
  return label;
}

/** The channel name an instrument's UI uses: D0.. for logic, CH1.. for a scope. */
export function channelName(label: string, channel: number): string {
  return /scope|oscillo/i.test(label) ? `CH${channel + 1}` : `D${channel}`;
}

/**
 * The board's test points with the probes on each. Where the shuffled
 * analyzer's channels are is a secret, but not which points it is clipped
 * to: those show as `"Logic #1 D?"`. `hiddenProbes` counts the probes whose
 * channel is to be found.
 */
export function testPointRows(status: SimStatus): { rows: TestPointRow[]; hiddenProbes: number } {
  const probes = new Map<string, string[]>();
  let hiddenProbes = 0;
  for (const inst of status.instruments) {
    const short = instrumentShortName(inst.label);
    const secret = inst.probes.filter((p) => !p.test_point).length;
    hiddenProbes += secret;
    if (secret > 0) {
      const unknown = /scope|oscillo/i.test(inst.label) ? "CH?" : "D?";
      for (const point of inst.probed_points) {
        const list = probes.get(point) ?? [];
        list.push(`${short} ${unknown}`);
        probes.set(point, list);
      }
    }
    for (const probe of inst.probes) {
      if (!probe.test_point) continue;
      const list = probes.get(probe.test_point) ?? [];
      list.push(`${short} ${channelName(inst.label, probe.channel)}`);
      probes.set(probe.test_point, list);
    }
  }
  const rows = status.board.test_points.map((tp) => ({
    id: tp.id,
    name: tp.name,
    description: tp.description,
    kind: tp.kind,
    probes: probes.get(tp.id) ?? [],
  }));
  return { rows, hiddenProbes };
}

/** Whole seconds as `1 h 02 min`, `4 min 05 s` or `12.3 s`. */
export function formatUptime(seconds: number | null | undefined): string {
  if (seconds === null || seconds === undefined) return "not running";
  if (seconds < 60) return `${seconds.toFixed(1)} s`;
  const s = Math.floor(seconds);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  if (h > 0) return `${h} h ${String(m).padStart(2, "0")} min`;
  return `${m} min ${String(sec).padStart(2, "0")} s`;
}

/** The pot's position as a slider value, on a log scale (470 Ω .. 100 kΩ). */
export const R2_MIN = 470;
export const R2_MAX = 100_000;

export function r2ToSlider(ohm: number): number {
  const clamped = Math.min(R2_MAX, Math.max(R2_MIN, ohm));
  return Math.round((1000 * Math.log(clamped / R2_MIN)) / Math.log(R2_MAX / R2_MIN));
}

export function sliderToR2(value: number): number {
  const ohm = R2_MIN * Math.pow(R2_MAX / R2_MIN, value / 1000);
  // Two significant figures, like a pot's scale.
  const digits = Math.pow(10, Math.floor(Math.log10(ohm)) - 1);
  return Math.round(ohm / digits) * digits;
}

export function formatOhm(ohm: number): string {
  return ohm >= 1000 ? `${(ohm / 1000).toFixed(ohm >= 10_000 ? 0 : 1)} kΩ` : `${Math.round(ohm)} Ω`;
}

export function formatHz(hz: number): string {
  return hz >= 1000 ? `${(hz / 1000).toFixed(2)} kHz` : `${hz.toFixed(0)} Hz`;
}
