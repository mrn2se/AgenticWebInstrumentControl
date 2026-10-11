import { describe, expect, it } from "vitest";
import { layoutAnnotations, levelRowBand, type AnnotationBoxInput } from "./annotation-layout";

const viewport = { t0: 0, t1: 10 };

describe("layoutAnnotations", () => {
  it("places a box proportionally within the pixel width", () => {
    const boxes: AnnotationBoxInput[] = [{ t0: 2, t1: 4, level: 0, label: "0x41" }];
    const [box] = layoutAnnotations(boxes, { widthPx: 1000, viewport });
    expect(box!.x0).toBeCloseTo(200, 6);
    expect(box!.x1).toBeCloseTo(400, 6);
  });

  it("drops a box entirely outside the viewport", () => {
    const boxes: AnnotationBoxInput[] = [{ t0: 20, t1: 21, level: 0, label: "x" }];
    expect(layoutAnnotations(boxes, { widthPx: 1000, viewport })).toEqual([]);
  });

  it("clips a box straddling the viewport's edge to [0, widthPx]", () => {
    const boxes: AnnotationBoxInput[] = [
      { t0: -5, t1: 2, level: 0, label: "left" },
      { t0: 9, t1: 15, level: 0, label: "right" },
    ];
    const laid = layoutAnnotations(boxes, { widthPx: 1000, viewport });
    expect(laid[0]!.x0).toBe(0);
    expect(laid[0]!.x1).toBeCloseTo(200, 6);
    expect(laid[1]!.x0).toBeCloseTo(900, 6);
    expect(laid[1]!.x1).toBe(1000);
  });

  it("drops a box that clips to a zero-or-negative-width sliver", () => {
    // Ends exactly at the viewport start: clipped x1 <= x0.
    const boxes: AnnotationBoxInput[] = [{ t0: -5, t1: 0, level: 0, label: "gone" }];
    expect(layoutAnnotations(boxes, { widthPx: 1000, viewport })).toEqual([]);
  });

  it("returns nothing for a degenerate viewport or width", () => {
    const boxes: AnnotationBoxInput[] = [{ t0: 0, t1: 1, level: 0, label: "x" }];
    expect(layoutAnnotations(boxes, { widthPx: 0, viewport })).toEqual([]);
    expect(layoutAnnotations(boxes, { widthPx: 100, viewport: { t0: 5, t1: 5 } })).toEqual([]);
  });

  describe("label fitting", () => {
    it("shows a label when the estimated text width fits the box", () => {
      // 4 chars * default 6px = 24px; box is 100px wide at this zoom.
      const boxes: AnnotationBoxInput[] = [{ t0: 0, t1: 1, level: 0, label: "0x41" }];
      const [box] = layoutAnnotations(boxes, { widthPx: 1000, viewport });
      expect(box!.showLabel).toBe(true);
    });

    it("hides a label when the box is narrower than the estimated text but still above the minimum", () => {
      // Box is 15px wide (above the 10px minimum) but the 6-char label
      // estimates to 36px — too wide for this box specifically.
      const boxes: AnnotationBoxInput[] = [{ t0: 0, t1: 0.15, level: 0, label: "0x4141" }];
      const [box] = layoutAnnotations(boxes, { widthPx: 1000, viewport });
      expect(box!.x1 - box!.x0).toBeCloseTo(15, 6);
      expect(box!.showLabel).toBe(false);
    });

    it("hides a label below the absolute minimum width even if the text would technically fit", () => {
      const boxes: AnnotationBoxInput[] = [{ t0: 0, t1: 0.05, level: 0, label: "x" }];
      const laid = layoutAnnotations(boxes, { widthPx: 1000, viewport, minLabelWidthPx: 20 });
      expect(laid[0]!.showLabel).toBe(false);
    });

    it("respects a custom char width estimate", () => {
      const boxes: AnnotationBoxInput[] = [{ t0: 0, t1: 1, level: 0, label: "0x41" }];
      const narrow = layoutAnnotations(boxes, { widthPx: 1000, viewport, charWidthPx: 1 });
      const wide = layoutAnnotations(boxes, { widthPx: 1000, viewport, charWidthPx: 1000 });
      expect(narrow[0]!.showLabel).toBe(true);
      expect(wide[0]!.showLabel).toBe(false);
    });
  });
});

describe("levelRowBand", () => {
  it("splits a lane into equal, stacked, non-overlapping bands per level", () => {
    const top0 = levelRowBand(0, 100, 130);
    const top1 = levelRowBand(1, 100, 130);
    const top2 = levelRowBand(2, 100, 130);
    expect(top0).toEqual({ top: 100, bottom: 110 });
    expect(top1).toEqual({ top: 110, bottom: 120 });
    expect(top2).toEqual({ top: 120, bottom: 130 });
  });

  it("clamps an out-of-range level into the last row rather than drawing outside the lane", () => {
    const band = levelRowBand(99, 0, 30);
    expect(band).toEqual({ top: 20, bottom: 30 });
  });

  it("clamps a negative level into the first row", () => {
    const band = levelRowBand(-1, 0, 30);
    expect(band).toEqual({ top: 0, bottom: 10 });
  });

  it("never produces a negative-height lane even if bottom < top", () => {
    const band = levelRowBand(0, 50, 10);
    expect(band.bottom - band.top).toBe(0);
  });
});
