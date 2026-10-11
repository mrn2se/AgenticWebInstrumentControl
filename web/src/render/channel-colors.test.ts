import { describe, expect, it } from "vitest";
import { channelColor, channelColorCss, contrastRatio, relativeLuminance } from "./channel-colors";

/** SPEC §11.1's fixed theme backgrounds (`--color-awic-bg`, `styles/index.css`). */
const LIGHT_BG = "#f7f7f8";
const DARK_BG = "#16171a";

/** WCAG SC 1.4.11 "non-text contrast" minimum — the bar set for the channel palette. */
const MIN_CONTRAST = 3;

describe("relativeLuminance / contrastRatio (WCAG)", () => {
  it("gives black and white the extreme luminances", () => {
    expect(relativeLuminance("#000000")).toBeCloseTo(0, 5);
    expect(relativeLuminance("#ffffff")).toBeCloseTo(1, 5);
  });

  it("is symmetric and maximal (21:1) for black against white", () => {
    expect(contrastRatio("#000000", "#ffffff")).toBeCloseTo(21, 0);
    expect(contrastRatio("#ffffff", "#000000")).toBeCloseTo(21, 0);
  });

  it("is 1:1 for identical colors", () => {
    expect(contrastRatio("#a85a00", "#a85a00")).toBeCloseTo(1, 5);
  });
});

describe("channelColorCss (theme-aware palette, >=3:1 in both themes)", () => {
  it("every channel entry clears 3:1 against the light background", () => {
    for (let channel = 0; channel < 8; channel++) {
      const hex = channelColorCss(channel, LIGHT_BG);
      const ratio = contrastRatio(hex, LIGHT_BG);
      expect(ratio, `channel ${channel} (${hex}) vs light bg ${LIGHT_BG}`).toBeGreaterThanOrEqual(MIN_CONTRAST);
    }
  });

  it("every channel entry clears 3:1 against the dark background", () => {
    for (let channel = 0; channel < 8; channel++) {
      const hex = channelColorCss(channel, DARK_BG);
      const ratio = contrastRatio(hex, DARK_BG);
      expect(ratio, `channel ${channel} (${hex}) vs dark bg ${DARK_BG}`).toBeGreaterThanOrEqual(MIN_CONTRAST);
    }
  });

  it("CH1 (channel 0) specifically is no longer too pale on the light theme", () => {
    const hex = channelColorCss(0, LIGHT_BG);
    expect(contrastRatio(hex, LIGHT_BG)).toBeGreaterThanOrEqual(MIN_CONTRAST);
  });

  it("picks a different shade per theme for the same channel", () => {
    expect(channelColorCss(0, LIGHT_BG)).not.toBe(channelColorCss(0, DARK_BG));
  });

  it("cycles for a channel number beyond the palette length, same as before", () => {
    expect(channelColorCss(8, LIGHT_BG)).toBe(channelColorCss(0, LIGHT_BG));
    expect(channelColorCss(-1, LIGHT_BG)).toBe(channelColorCss(7, LIGHT_BG));
  });
});

describe("channelColor (RGBA float form)", () => {
  it("carries the requested alpha through unchanged", () => {
    const [, , , a] = channelColor(0, LIGHT_BG, 0.35);
    expect(a).toBe(0.35);
  });

  it("defaults to fully opaque", () => {
    const [, , , a] = channelColor(0, LIGHT_BG);
    expect(a).toBe(1);
  });

  it("matches channelColorCss's hex, converted to [0,1] floats", () => {
    const hex = channelColorCss(3, DARK_BG);
    const [r, g, b] = channelColor(3, DARK_BG);
    expect(r).toBeCloseTo(parseInt(hex.slice(1, 3), 16) / 255, 5);
    expect(g).toBeCloseTo(parseInt(hex.slice(3, 5), 16) / 255, 5);
    expect(b).toBeCloseTo(parseInt(hex.slice(5, 7), 16) / 255, 5);
  });
});
