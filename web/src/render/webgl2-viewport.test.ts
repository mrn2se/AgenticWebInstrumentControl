import { describe, expect, it } from "vitest";
import {
  clampViewport,
  fitAllViewport,
  panViewport,
  pointerXToTime,
  timeToPointerX,
  traceSetTimeBounds,
  wheelAction,
  wheelDeltaPixels,
  zoomViewport,
} from "./webgl2-viewport";
import type { Trace } from "./types";

describe("zoomViewport", () => {
  it("shrinks the span and keeps the pivot fixed on screen when zooming in", () => {
    const v = zoomViewport({ t0: 0, t1: 10 }, 4, 0.5);
    // pivot (4) is 40% across the original [0,10] span; the new 5-wide span keeps that ratio.
    expect(v.t1 - v.t0).toBeCloseTo(5, 10);
    expect(4 - v.t0).toBeCloseTo((4 - 0) / 10 * 5, 10);
  });

  it("grows the span when zooming out", () => {
    const v = zoomViewport({ t0: 0, t1: 10 }, 0, 2);
    expect(v.t1 - v.t0).toBeCloseTo(20, 10);
    expect(v.t0).toBeCloseTo(0, 10);
  });

  it("never collapses the span to zero or negative", () => {
    const v = zoomViewport({ t0: 0, t1: 1e-15 }, 0, 0.001);
    expect(v.t1).toBeGreaterThan(v.t0);
  });

  it("is a no-op on a degenerate (zero-width) viewport", () => {
    const v = zoomViewport({ t0: 5, t1: 5 }, 5, 0.5);
    expect(v).toEqual({ t0: 5, t1: 5 });
  });
});

describe("panViewport", () => {
  it("shifts both bounds by the same delta, preserving the span", () => {
    const v = panViewport({ t0: 0, t1: 10 }, 3);
    expect(v).toEqual({ t0: 3, t1: 13 });
  });

  it("pans backwards with a negative delta", () => {
    expect(panViewport({ t0: 0, t1: 10 }, -3)).toEqual({ t0: -3, t1: 7 });
  });
});

describe("pointer <-> time mapping", () => {
  const viewport = { t0: 100, t1: 200 };

  it("maps the left edge to t0 and the right edge to t1", () => {
    expect(pointerXToTime(0, 1000, viewport)).toBeCloseTo(100, 10);
    expect(pointerXToTime(1000, 1000, viewport)).toBeCloseTo(200, 10);
  });

  it("maps the midpoint to the viewport's midpoint", () => {
    expect(pointerXToTime(500, 1000, viewport)).toBeCloseTo(150, 10);
  });

  it("is the exact inverse of timeToPointerX", () => {
    const x = timeToPointerX(137, 1000, viewport);
    expect(pointerXToTime(x, 1000, viewport)).toBeCloseTo(137, 6);
  });

  it("degrades gracefully for a zero-width canvas or viewport", () => {
    expect(pointerXToTime(5, 0, viewport)).toBe(viewport.t0);
    expect(timeToPointerX(150, 1000, { t0: 5, t1: 5 })).toBe(0);
  });
});

describe("traceSetTimeBounds", () => {
  it("returns null for an empty trace set", () => {
    expect(traceSetTimeBounds([])).toBeNull();
  });

  it("spans an analog trace's own t[] range", () => {
    const traces: Trace[] = [{ kind: "analog", channel: 0, label: "CH1", t: [1, 2, 3], v: [0, 0, 0] }];
    expect(traceSetTimeBounds(traces)).toEqual({ t0: 1, t1: 3 });
  });

  it("spans an envelope trace's bucket edges, t0[] to t1[]", () => {
    const traces: Trace[] = [
      { kind: "analog-envelope", channel: 0, label: "CH1", t0: [0, 1], t1: [1, 2], vMin: [0, 0], vMax: [1, 1] },
    ];
    expect(traceSetTimeBounds(traces)).toEqual({ t0: 0, t1: 2 });
  });

  it("spans a digital trace's segment edges", () => {
    const traces: Trace[] = [
      {
        kind: "digital",
        channel: 0,
        label: "D0",
        lane: 0,
        segments: [
          { t0: 0, t1: 1, state: "low" },
          { t0: 1, t1: 5, state: "high" },
        ],
      },
    ];
    expect(traceSetTimeBounds(traces)).toEqual({ t0: 0, t1: 5 });
  });

  it("unions bounds across multiple traces", () => {
    const traces: Trace[] = [
      { kind: "analog", channel: 0, label: "CH1", t: [5, 10], v: [0, 0] },
      { kind: "analog", channel: 1, label: "CH2", t: [0, 2], v: [0, 0] },
    ];
    expect(traceSetTimeBounds(traces)).toEqual({ t0: 0, t1: 10 });
  });
});

describe("fitAllViewport", () => {
  it("pads a non-degenerate range on both sides", () => {
    const v = fitAllViewport({ t0: 0, t1: 100 }, 0.1);
    expect(v.t0).toBeCloseTo(-10, 10);
    expect(v.t1).toBeCloseTo(110, 10);
  });

  it("falls back to a fixed 1-unit pad for a zero-width range", () => {
    const v = fitAllViewport({ t0: 5, t1: 5 });
    expect(v.t0).toBeCloseTo(4, 10);
    expect(v.t1).toBeCloseTo(6, 10);
  });
});

describe("clampViewport", () => {
  const extent = { t0: 0, t1: 100 };

  it("leaves a viewport inside the data alone", () => {
    expect(clampViewport({ t0: 10, t1: 20 }, extent)).toEqual({ t0: 10, t1: 20 });
  });

  it("caps zoom-out at the fit-all span", () => {
    const v = clampViewport({ t0: -50, t1: 150 }, extent);
    expect(v.t0).toBeCloseTo(-2, 10);
    expect(v.t1).toBeCloseTo(102, 10);
  });

  it("slides a pan past either edge back inside, keeping the span", () => {
    const left = clampViewport({ t0: -40, t1: -30 }, extent);
    expect(left.t0).toBeCloseTo(-2, 10);
    expect(left.t1 - left.t0).toBeCloseTo(10, 10);
    const right = clampViewport({ t0: 500, t1: 510 }, extent);
    expect(right.t1).toBeCloseTo(102, 10);
    expect(right.t1 - right.t0).toBeCloseTo(10, 10);
  });

  it("does nothing without an extent", () => {
    expect(clampViewport({ t0: -40, t1: -30 }, null)).toEqual({ t0: -40, t1: -30 });
  });
});

describe("wheelDeltaPixels", () => {
  it("passes pixel deltas through", () => {
    expect(wheelDeltaPixels(53, 0, 800)).toBe(53);
  });

  it("scales line deltas to pixels", () => {
    expect(wheelDeltaPixels(3, 1, 800)).toBe(48);
  });

  it("scales page deltas by the page size", () => {
    expect(wheelDeltaPixels(1, 2, 800)).toBe(800);
  });
});

describe("wheelAction", () => {
  const base = { dx: 0, dy: 0, shiftKey: false, ctrlKey: false };

  it("zooms out on a plain wheel down and in on a wheel up", () => {
    const out = wheelAction({ ...base, dy: 100 });
    const inward = wheelAction({ ...base, dy: -100 });
    expect(out.kind === "zoom" && out.factor > 1).toBe(true);
    expect(inward.kind === "zoom" && inward.factor < 1).toBe(true);
  });

  it("pans on a horizontal swipe", () => {
    expect(wheelAction({ ...base, dx: 40, dy: 3 })).toEqual({ kind: "pan", px: 40 });
  });

  it("pans on Shift+wheel, whichever axis the browser reports it on", () => {
    expect(wheelAction({ ...base, dy: 48, shiftKey: true })).toEqual({ kind: "pan", px: 48 });
    expect(wheelAction({ ...base, dx: 48, shiftKey: true })).toEqual({ kind: "pan", px: 48 });
  });

  it("zooms on a pinch (Ctrl) even with a horizontal component", () => {
    expect(wheelAction({ ...base, dx: 40, dy: 5, ctrlKey: true }).kind).toBe("zoom");
  });

  it("does nothing for an empty event", () => {
    expect(wheelAction(base)).toEqual({ kind: "none" });
  });
});
