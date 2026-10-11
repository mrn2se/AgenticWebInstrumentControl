/**
 * Registers every built-in renderer backend as an import side effect — the
 * same seam pattern as `shell/registry.ts`'s module registration. Importing
 * this barrel is what makes all three ids available from
 * `createRenderer`/`listRenderers`; `plotly.ts` itself performs no top-level
 * work beyond registering a factory, so this import does not pull the
 * Plotly library into the loading module's chunk (see `render/plotly.ts`).
 */
import "./plotly";
import "./webgl2";
import "./uplot";

export type {
  AnalogEnvelopeTrace,
  AnalogPanelSpec,
  AnalogTrace,
  DigitalPanelSpec,
  DigitalSegment,
  DigitalTrace,
  Layout,
  PanelSpec,
  RendererId,
  Trace,
  TraceSet,
  WaveformRenderer,
} from "./types";
export { RendererNotImplementedError, RendererUnavailableError } from "./types";
export {
  DEFAULT_RENDERER_ID,
  createRenderer,
  isRendererRegistered,
  listRenderers,
  registerRenderer,
} from "./registry";
