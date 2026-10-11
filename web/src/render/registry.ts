import type { RendererId, WaveformRenderer } from "./types";

/**
 * The renderer registration seam (SPECIFICATION.md §5.3): each backend file
 * (`plotly.ts`, `webgl2.ts`, `uplot.ts`) calls `registerRenderer` as an
 * import side effect (see `render/index.ts`), the same pattern
 * `shell/registry.ts` uses for modules. The graph module never imports a
 * backend directly — it asks this registry for whichever `RendererId` the
 * user (or `[renderer] default` in config) selected, which is what makes
 * the choice switchable at runtime.
 */
export type RendererFactory = () => WaveformRenderer;

const factories = new Map<RendererId, RendererFactory>();

export function registerRenderer(id: RendererId, factory: RendererFactory): void {
  if (factories.has(id)) {
    throw new Error(`renderer "${id}" is already registered`);
  }
  factories.set(id, factory);
}

/** Constructs a fresh renderer instance for `id`. Throws if nothing registered it. */
export function createRenderer(id: RendererId): WaveformRenderer {
  const factory = factories.get(id);
  if (!factory) {
    throw new Error(`renderer "${id}" is not registered`);
  }
  return factory();
}

export function isRendererRegistered(id: RendererId): boolean {
  return factories.has(id);
}

/** All registered renderer ids, in registration order. */
export function listRenderers(): readonly RendererId[] {
  return [...factories.keys()];
}

/** Test-only: clears the registry so each test file starts from empty. */
export function resetRegistryForTests(): void {
  factories.clear();
}

/** SPEC §5.3, D-05: WebGL2 is the default renderer (revising R-06) — Plotly stays selectable. */
export const DEFAULT_RENDERER_ID: RendererId = "webgl2";
