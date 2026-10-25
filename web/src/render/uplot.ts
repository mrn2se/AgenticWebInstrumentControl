import { registerRenderer } from "./registry";
import type { AnnotationLane, Layout, ThemeColors, TimeRange, TraceSet, TriggerMarker, WaveformRenderer } from "./types";
import { RendererNotImplementedError } from "./types";

/**
 * Registration seam for the `uplot` backend (SPEC §5.3): "tiny, excellent to
 * ~100 k points". Not built yet — see `render/webgl2.ts` for why this
 * throws rather than half-rendering.
 */
class UplotRenderer implements WaveformRenderer {
  readonly id = "uplot" as const;

  mount(_el: HTMLElement, _layout: Layout): void {
    throw new RendererNotImplementedError(this.id);
  }

  /** Nothing is ever mounted, so there is no layout to replace. */
  setLayout(_layout: Layout): void {
    // Same reasoning as `setAnnotations`: the graph module should not have to
    // ask which backend is mounted before adjusting a lane.
  }

  setTraces(_traces: TraceSet): void {
    throw new RendererNotImplementedError(this.id);
  }

  setAnnotations(_lanes: readonly AnnotationLane[]): void {
    // Nothing is ever mounted; nothing to draw onto.
  }

  setViewport(_t0: number, _t1: number): void {
    throw new RendererNotImplementedError(this.id);
  }

  setTrigger(_marker: TriggerMarker | null): void {
    // Nothing is ever mounted; nothing to draw onto.
  }

  setTimeLimits(_range: TimeRange | null): void {
    // Nothing is ever mounted; there is no zoom to limit.
  }

  /** Nothing is ever mounted, so nothing ever fires; still returns a working unsubscribe function. */
  onViewportChange(_cb: (t0: number, t1: number) => void): () => void {
    return () => {
      // Never subscribed anything — nothing to remove.
    };
  }

  setTheme(_colors: ThemeColors): void {
    // Nothing was ever mounted; there's nothing to re-color.
  }

  destroy(): void {
    // Nothing was ever mounted; destroy is always safe to call.
  }
}

registerRenderer("uplot", () => new UplotRenderer());
