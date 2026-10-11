/**
 * One consistent hue per channel (SPEC §5.3), independent of panel kind, so
 * a device with several analog or digital channels reads as distinct traces
 * at a glance. `webgl2.ts` uses this directly; `plotly-mapping.ts` currently
 * draws every analog channel in one fixed color and every digital channel in
 * another (SPEC's "share the palette with Plotly if sensible" is a nice-to
 * -have, not a requirement, and changing `plotly-mapping.ts`'s established,
 * tested output was out of this task's scope) — the palette lives here,
 * factored out, so Plotly can adopt it later without duplicating the colors.
 *
 * A small, high-contrast categorical palette (Okabe-Ito, colorblind-safe),
 * cycling by channel number — **theme-aware** ("the CH1 hue is
 * too pale on the light theme"). A hue tuned to read clearly on a dark plot
 * background is, almost by construction, too light to clear WCAG's 3:1
 * "non-text contrast" minimum against a light one (and vice versa), so one
 * fixed palette cannot serve both themes — `#e69f00` (the original CH1
 * orange) measures 2.10:1 against this app's light background, nowhere near
 * 3:1. Each theme therefore gets its own shade of every hue, picked so every
 * entry clears 3:1 against *that* theme's plot background
 * (`channel-colors.test.ts` checks all sixteen).
 */
const PALETTE_LIGHT = [
  "#a85a00", // orange (darkened from #e69f00 for the light background)
  "#0077b6", // sky blue
  "#007a53", // bluish green
  "#8a6d00", // yellow (olive on light — pure yellow can't clear 3:1 on any light ground)
  "#004c8c", // blue
  "#b03a00", // vermillion
  "#9c2f72", // reddish purple
  "#000000", // black
] as const;

const PALETTE_DARK = [
  "#e69f00", // orange
  "#56b4e9", // sky blue
  "#3ddbb0", // bluish green (lightened from #009e73 for the dark background)
  "#f0e442", // yellow
  "#7fb8ff", // blue (lightened from #0072b2)
  "#ff8a5c", // vermillion (lightened from #d55e00)
  "#e6a8d7", // reddish purple
  "#ffffff", // white (black cannot show on a dark background at all)
] as const;

/** RGBA in `[0, 1]` for `gl.uniform4fv` / `vertexAttrib4f`, per WebGL convention. */
export type RgbaFloat = readonly [number, number, number, number];

function hexToRgb01(hex: string): readonly [number, number, number] {
  const r = parseInt(hex.slice(1, 3), 16) / 255;
  const g = parseInt(hex.slice(3, 5), 16) / 255;
  const b = parseInt(hex.slice(5, 7), 16) / 255;
  return [r, g, b];
}

function hexToRgbaFloat(hex: string, alpha: number): RgbaFloat {
  const [r, g, b] = hexToRgb01(hex);
  return [r, g, b, alpha];
}

/** WCAG relative luminance (used both to classify a background as light/dark and to compute contrast ratios). */
export function relativeLuminance(hex: string): number {
  const linearize = (c: number): number => (c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4);
  const [r, g, b] = hexToRgb01(hex).map(linearize);
  return 0.2126 * r! + 0.7152 * g! + 0.0722 * b!;
}

/** WCAG contrast ratio between two colors, in `[1, 21]`. */
export function contrastRatio(hexA: string, hexB: string): number {
  const [lighter, darker] = [relativeLuminance(hexA), relativeLuminance(hexB)].sort((a, b) => b - a);
  return (lighter! + 0.05) / (darker! + 0.05);
}

/**
 * Picks the palette for a given plot background: whichever of the two
 * fixed backgrounds (SPEC §11.1's light/dark theme, `--color-awic-bg`) it is
 * closer to in luminance, defaulting to light for anything else (e.g. a
 * plain white fallback before a theme has been applied).
 */
function paletteFor(background: string): readonly string[] {
  return relativeLuminance(background) < 0.5 ? PALETTE_DARK : PALETTE_LIGHT;
}

function paletteEntry(channel: number, background: string): string {
  const palette = paletteFor(background);
  return palette[((channel % palette.length) + palette.length) % palette.length]!;
}

export function channelColor(channel: number, background: string, alpha = 1): RgbaFloat {
  return hexToRgbaFloat(paletteEntry(channel, background), alpha);
}

export function channelColorCss(channel: number, background: string): string {
  return paletteEntry(channel, background);
}
