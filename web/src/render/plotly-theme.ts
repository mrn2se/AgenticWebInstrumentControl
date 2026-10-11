import type { ThemeColors } from "./types";

/**
 * Reads AWIC's own theme custom properties (`--color-awic-bg`,
 * `--color-awic-fg`, ..., declared once via Tailwind's `@theme` in
 * `styles/index.css`, SPEC §11.1) off a live element's computed style,
 * rather than keeping a second light/dark color table in JS. Custom
 * properties inherit normally through light DOM (see `shell/module.ts`), so
 * this resolves correctly from inside a mounted module — whatever the
 * current theme (light, dark, or system, and however "system" was decided),
 * this is always in sync with it because it asks the cascade directly
 * instead of guessing.
 *
 * DOM-only by design (`getComputedStyle`), so it is not unit-tested here;
 * `plotly-mapping.test.ts` covers what happens with a given `ThemeColors`
 * value, which is the part worth testing without a browser.
 */
export function readThemeColors(el: Element): ThemeColors {
  const style = getComputedStyle(el);
  const read = (name: string, fallback: string): string => style.getPropertyValue(name).trim() || fallback;
  return {
    background: read("--color-awic-bg", "#ffffff"),
    foreground: read("--color-awic-fg", "#16171a"),
    mutedForeground: read("--color-awic-fg-muted", "#5b5d63"),
    border: read("--color-awic-border", "#d8d8dc"),
  };
}
