import { describe, expect, it } from "vitest";
import { analogPanelBounds, analogPanelSpan, panelRowClip, panelScissorRect, timeAxisTransform, valueAxisTransform } from "./webgl2-panel-transform";

describe("panelRowClip", () => {
  it("gives the single panel the full clip-space height, minus its own gap", () => {
    const row = panelRowClip(0, 1);
    expect(row.top).toBeCloseTo(1 - 0.06, 10);
    expect(row.bottom).toBeCloseTo(-1 + 0.06, 10);
  });

  it("stacks panels top to bottom without overlapping", () => {
    const a = panelRowClip(0, 2);
    const b = panelRowClip(1, 2);
    expect(a.top).toBeGreaterThan(a.bottom);
    expect(b.top).toBeGreaterThan(b.bottom);
    expect(a.bottom).toBeGreaterThanOrEqual(b.top); // a gap, never an overlap
  });

  it("gives every row of an n-panel layout the same height", () => {
    const heights = [0, 1, 2, 3].map((i) => {
      const row = panelRowClip(i, 4);
      return row.top - row.bottom;
    });
    for (const h of heights) expect(h).toBeCloseTo(heights[0]!, 10);
  });
});

describe("valueAxisTransform", () => {
  it("maps the range's midpoint to the row's vertical center", () => {
    const t = valueAxisTransform(1, -1, 10, 2); // range [-3, 7], offset 2
    const clipAtMid = 2 * t.scale + t.offset;
    expect(clipAtMid).toBeCloseTo(0, 10);
  });

  it("maps offset - range/2 to the row bottom and offset + range/2 to the row top", () => {
    const t = valueAxisTransform(1, -1, 10, 0);
    expect(-5 * t.scale + t.offset).toBeCloseTo(-1, 10);
    expect(5 * t.scale + t.offset).toBeCloseTo(1, 10);
  });

  it("does not divide by zero for a non-positive range", () => {
    const t = valueAxisTransform(1, -1, 0, 5);
    expect(Number.isFinite(t.scale)).toBe(true);
    expect(Number.isFinite(t.offset)).toBe(true);
  });
});

describe("timeAxisTransform", () => {
  it("maps t0 to clip -1 and t1 to clip +1, honoring the origin shift", () => {
    const origin = 1000;
    const t = timeAxisTransform(1000.0001, 1000.0003, origin);
    const localT0 = 1000.0001 - origin;
    const localT1 = 1000.0003 - origin;
    expect(localT0 * t.scale + t.offset).toBeCloseTo(-1, 6);
    expect(localT1 * t.scale + t.offset).toBeCloseTo(1, 6);
  });

  it("does not divide by zero for a degenerate viewport", () => {
    const t = timeAxisTransform(5, 5, 0);
    expect(Number.isFinite(t.scale)).toBe(true);
    expect(Number.isFinite(t.offset)).toBe(true);
  });
});

describe("analogPanelBounds", () => {
  // SPEC §5.2: `ranges_v` is a ± full-scale range. The oscilloscope's 5 V range
  // returns code 0 as -5 V and code 255 as +5 V; a lane drawn to +2.5 V put a
  // 2.7-4.1 V CAN pair outside it (B-55).
  it("spans a full ± rangeV around the offset", () => {
    expect(analogPanelBounds(5, 0)).toEqual({ min: -5, max: 5 });
    expect(analogPanelBounds(2.5, 0)).toEqual({ min: -2.5, max: 2.5 });
    expect(analogPanelBounds(5, 2.5)).toEqual({ min: -2.5, max: 7.5 });
    expect(analogPanelSpan(5)).toBe(10);
  });

  it("puts a 2.7-4.1 V signal inside the 5 V lane, and a 2.5 V lane leaves it out", () => {
    const five = analogPanelBounds(5, 0);
    expect(five.min).toBeLessThanOrEqual(2.7);
    expect(five.max).toBeGreaterThanOrEqual(4.1);
    expect(analogPanelBounds(2.5, 0).max).toBeLessThan(4.1);
  });
});

describe("panelScissorRect", () => {
  it("covers each row once, in pixels from the bottom", () => {
    const pxH = 400;
    const top = panelScissorRect(0, 2, 800, pxH);
    const bottom = panelScissorRect(1, 2, 800, pxH);
    expect(top.width).toBe(800);
    // Row 0 is the upper half of the canvas: its box is the upper half in
    // GL's bottom-up pixels, so it starts above the midpoint.
    expect(top.y).toBeGreaterThanOrEqual(pxH / 2);
    expect(bottom.y).toBeLessThan(pxH / 2);
    // Neither box reaches into the other: a trace off the top of its lane is
    // clipped rather than drawn over its neighbour.
    expect(bottom.y + bottom.height).toBeLessThanOrEqual(top.y);
    for (const box of [top, bottom]) {
      expect(box.height).toBeGreaterThan(0);
      expect(box.y).toBeGreaterThanOrEqual(0);
      expect(box.y + box.height).toBeLessThanOrEqual(pxH);
    }
  });

  it("is the whole canvas for a single panel, and never negative", () => {
    // Minus the row gap `panelRowClip` leaves, 3 % of the row at each edge.
    const only = panelScissorRect(0, 1, 640, 200);
    expect(only.y).toBe(6);
    expect(only.height).toBe(188);
    const degenerate = panelScissorRect(0, 0, 0, 0);
    expect(degenerate.width).toBe(0);
    expect(degenerate.height).toBe(0);
    expect(degenerate.y).toBe(0);
  });
});
