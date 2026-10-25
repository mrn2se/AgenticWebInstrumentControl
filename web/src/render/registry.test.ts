import { beforeEach, describe, expect, it } from "vitest";
import {
  DEFAULT_RENDERER_ID,
  createRenderer,
  isRendererRegistered,
  listRenderers,
  registerRenderer,
  resetRegistryForTests,
} from "./registry";
import type { AnnotationLane, Layout, ThemeColors, TraceSet, WaveformRenderer } from "./types";

class FakeRenderer implements WaveformRenderer {
  readonly id;
  mounted: HTMLElement | null = null;
  lastTraces: TraceSet | null = null;
  lastViewport: { t0: number; t1: number } | null = null;
  lastTheme: ThemeColors | null = null;
  destroyed = false;

  constructor(id: WaveformRenderer["id"]) {
    this.id = id;
  }

  lastLayout: Layout | null = null;

  mount(el: HTMLElement, layout: Layout): void {
    this.mounted = el;
    this.lastLayout = layout;
  }
  setLayout(layout: Layout): void {
    this.lastLayout = layout;
  }
  setTraces(traces: TraceSet): void {
    this.lastTraces = traces;
  }
  setAnnotations(_lanes: readonly AnnotationLane[]): void {
    // Not exercised by these registry tests.
  }
  setTrigger(): void {
    // Not exercised by these registry tests.
  }
  setTimeLimits(): void {
    // Not exercised by these registry tests.
  }
  setViewport(t0: number, t1: number): void {
    this.lastViewport = { t0, t1 };
  }
  onViewportChange(_cb: (t0: number, t1: number) => void): () => void {
    return () => {
      // Fake never fires it — nothing to unsubscribe.
    };
  }
  setTheme(colors: ThemeColors): void {
    this.lastTheme = colors;
  }
  destroy(): void {
    this.destroyed = true;
  }
}

describe("renderer registry", () => {
  beforeEach(() => {
    resetRegistryForTests();
  });

  it("has no renderers registered until backends import themselves", () => {
    expect(listRenderers()).toEqual([]);
    expect(isRendererRegistered("plotly")).toBe(false);
  });

  it("registers a factory and constructs a fresh instance per call", () => {
    let constructed = 0;
    registerRenderer("plotly", () => {
      constructed++;
      return new FakeRenderer("plotly");
    });

    const a = createRenderer("plotly");
    const b = createRenderer("plotly");
    expect(a).not.toBe(b);
    expect(constructed).toBe(2);
  });

  it("refuses to register the same id twice", () => {
    registerRenderer("plotly", () => new FakeRenderer("plotly"));
    expect(() => registerRenderer("plotly", () => new FakeRenderer("plotly"))).toThrow(/already registered/);
  });

  it("throws a clear error asking for an unregistered id", () => {
    expect(() => createRenderer("webgl2")).toThrow(/not registered/);
  });

  it("lists registered ids in registration order", () => {
    registerRenderer("uplot", () => new FakeRenderer("uplot"));
    registerRenderer("plotly", () => new FakeRenderer("plotly"));
    expect(listRenderers()).toEqual(["uplot", "plotly"]);
  });

  it("switches which backend is active at runtime by id, per SPEC §5.3", () => {
    registerRenderer("plotly", () => new FakeRenderer("plotly"));
    registerRenderer("webgl2", () => new FakeRenderer("webgl2"));

    let active: WaveformRenderer = createRenderer(DEFAULT_RENDERER_ID);
    expect(active.id).toBe("webgl2");

    active.destroy();
    active = createRenderer("plotly");
    expect(active.id).toBe("plotly");
  });

  it("defaults to webgl2 per D-05 (revising R-06)", () => {
    expect(DEFAULT_RENDERER_ID).toBe("webgl2");
  });
});
