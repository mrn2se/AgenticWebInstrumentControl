import { registerRenderer } from "./registry";
import { panelIndex } from "./plotly-mapping";
import type { AnnotationLane, Layout, ThemeColors, Trace, TraceSet, WaveformRenderer } from "./types";
import { RendererUnavailableError } from "./types";
import { layoutAnnotations, levelRowBand } from "./annotation-layout";
import { channelColor, type RgbaFloat } from "./channel-colors";
import {
  buildAnalogEnvelopeOutlineVertices,
  buildAnalogEnvelopeVertices,
  buildAnalogLineVertices,
  buildDigitalLevelVertices,
  buildDigitalToggleVertices,
} from "./webgl2-geometry";
import { formatSiTime, formatVolts, timeTicks, voltTicks } from "./webgl2-axis";
import {
  fitAllViewport,
  panViewport,
  pointerXToTime,
  timeToPointerX,
  traceSetTimeBounds,
  zoomViewport,
  type Viewport,
} from "./webgl2-viewport";
import {
  analogPanelBounds,
  analogPanelSpan,
  DIGITAL_OFFSET,
  DIGITAL_RANGE,
  panelRowClip,
  panelScissorRect,
  timeAxisTransform,
  valueAxisTransform,
} from "./webgl2-panel-transform";

/**
 * SPEC §5.3, D-05: the default waveform renderer. No dependencies — plain
 * WebGL2 plus a 2D canvas overlay for axes/grid/labels. The heavy pure math
 * (which vertices a trace becomes, axis ticks, viewport zoom/pan, the
 * panel-to-clip-space transform) lives in sibling `webgl2-*.ts` modules so it
 * can be unit-tested without a `<canvas>`; this file is the DOM/GL glue,
 * which — like `plotly.ts` — is exercised by the e2e smoke test instead.
 *
 * Fixed WebGL enum values (not read off a live `gl` object) so CPU-side
 * geometry can be described even while the context is lost (SPEC: "handle
 * WebGL context loss").
 */
const GL_LINES = 0x0001;
const GL_LINE_STRIP = 0x0003;
const GL_TRIANGLES = 0x0004;
const GL_TRIANGLE_STRIP = 0x0005;

const VERTEX_SHADER = `#version 300 es
layout(location = 0) in vec2 a_pos;
uniform float u_xScale;
uniform float u_xOffset;
uniform float u_yScale;
uniform float u_yOffset;
void main() {
  gl_Position = vec4(a_pos.x * u_xScale + u_xOffset, a_pos.y * u_yScale + u_yOffset, 0.0, 1.0);
}
`;

const FRAGMENT_SHADER = `#version 300 es
precision mediump float;
uniform vec4 u_color;
out vec4 outColor;
void main() {
  outColor = u_color;
}
`;

function compileShader(gl: WebGL2RenderingContext, type: number, source: string): WebGLShader {
  const shader = gl.createShader(type);
  if (!shader) throw new Error("webgl2 renderer: gl.createShader failed");
  gl.shaderSource(shader, source);
  gl.compileShader(shader);
  if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) {
    const info = gl.getShaderInfoLog(shader);
    gl.deleteShader(shader);
    throw new Error(`webgl2 renderer: shader compile failed: ${info ?? "unknown error"}`);
  }
  return shader;
}

function linkProgram(gl: WebGL2RenderingContext): WebGLProgram {
  const vertexShader = compileShader(gl, gl.VERTEX_SHADER, VERTEX_SHADER);
  const fragmentShader = compileShader(gl, gl.FRAGMENT_SHADER, FRAGMENT_SHADER);
  const program = gl.createProgram();
  if (!program) throw new Error("webgl2 renderer: gl.createProgram failed");
  gl.attachShader(program, vertexShader);
  gl.attachShader(program, fragmentShader);
  gl.linkProgram(program);
  gl.deleteShader(vertexShader);
  gl.deleteShader(fragmentShader);
  if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
    const info = gl.getProgramInfoLog(program);
    gl.deleteProgram(program);
    throw new Error(`webgl2 renderer: program link failed: ${info ?? "unknown error"}`);
  }
  return program;
}

/** Parses AWIC's theme hex colors (`--color-awic-*`, always `#rrggbb`) into `[0,1]` floats; a neutral gray if a token is ever something else. */
function parseHexColor(hex: string): readonly [number, number, number] {
  const match = /^#([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})$/i.exec(hex);
  if (!match) return [0.5, 0.5, 0.5];
  return [parseInt(match[1]!, 16) / 255, parseInt(match[2]!, 16) / 255, parseInt(match[3]!, 16) / 255];
}

interface PendingItem {
  readonly data: Float32Array;
  readonly primitive: number;
  readonly panel: number;
  readonly range: number;
  readonly offset: number;
  readonly color: RgbaFloat;
}

interface Drawable {
  readonly buffer: WebGLBuffer;
  readonly vertexCount: number;
  readonly primitive: number;
  readonly panel: number;
  readonly range: number;
  readonly offset: number;
  readonly color: RgbaFloat;
}

interface DragState {
  readonly pointerId: number;
  readonly startXCss: number;
  readonly startViewport: Viewport;
}

interface Uniforms {
  readonly xScale: WebGLUniformLocation;
  readonly xOffset: WebGLUniformLocation;
  readonly yScale: WebGLUniformLocation;
  readonly yOffset: WebGLUniformLocation;
  readonly color: WebGLUniformLocation;
}

class Webgl2Renderer implements WaveformRenderer {
  readonly id = "webgl2" as const;

  private el: HTMLElement | null = null;
  private glCanvas: HTMLCanvasElement | null = null;
  private overlayCanvas: HTMLCanvasElement | null = null;
  private gl: WebGL2RenderingContext | null = null;
  private overlayCtx: CanvasRenderingContext2D | null = null;
  private program: WebGLProgram | null = null;
  private uniforms: Uniforms | null = null;

  private layout: Layout = { panels: [] };
  private traces: TraceSet = { traces: [] };
  private annotationLanes: readonly AnnotationLane[] = [];
  /** CPU-side vertex data, rebuilt on every `setTraces()`; survives a lost GL context (SPEC: "re-create resources on restore"). */
  private pending: PendingItem[] = [];
  private drawables: Drawable[] = [];
  private originSeconds = 0;

  private viewport: Viewport | null = null;
  private theme: ThemeColors | null = null;

  private resizeObserver: ResizeObserver | null = null;
  private rafHandle: number | null = null;
  private readonly listeners = new Set<(t0: number, t1: number) => void>();
  private dragState: DragState | null = null;

  private readonly handleWheel = (e: WheelEvent): void => {
    if (!this.viewport || !this.glCanvas) return;
    e.preventDefault();
    const rect = this.glCanvas.getBoundingClientRect();
    const pivot = pointerXToTime(e.clientX - rect.left, rect.width, this.viewport);
    // deltaY > 0 (scroll down / pinch out) zooms out; a smooth exponential
    // response feels closer to a real scope's knob than a fixed step.
    const factor = Math.exp(e.deltaY * 0.001);
    this.viewport = zoomViewport(this.viewport, pivot, factor);
    this.notifyViewportChange();
    this.scheduleDraw();
  };

  private readonly handlePointerDown = (e: PointerEvent): void => {
    if (!this.viewport || !this.glCanvas) return;
    this.glCanvas.setPointerCapture(e.pointerId);
    this.dragState = { pointerId: e.pointerId, startXCss: e.clientX, startViewport: this.viewport };
  };

  private readonly handlePointerMove = (e: PointerEvent): void => {
    const drag = this.dragState;
    if (!drag || drag.pointerId !== e.pointerId || !this.glCanvas) return;
    const rect = this.glCanvas.getBoundingClientRect();
    const span = drag.startViewport.t1 - drag.startViewport.t0;
    const deltaSeconds = -((e.clientX - drag.startXCss) / Math.max(rect.width, 1)) * span;
    this.viewport = panViewport(drag.startViewport, deltaSeconds);
    this.notifyViewportChange();
    this.scheduleDraw();
  };

  private readonly handlePointerUp = (e: PointerEvent): void => {
    if (this.dragState?.pointerId === e.pointerId) this.dragState = null;
  };

  private readonly handleDoubleClick = (): void => {
    const bounds = traceSetTimeBounds(this.traces.traces);
    if (!bounds) return;
    this.viewport = fitAllViewport(bounds);
    this.notifyViewportChange();
    this.scheduleDraw();
  };

  private readonly handleContextLost = (e: Event): void => {
    e.preventDefault();
    this.deleteDrawables();
    this.gl = null;
    this.program = null;
    this.uniforms = null;
  };

  private readonly handleContextRestored = (): void => {
    if (!this.glCanvas) return;
    const gl = this.glCanvas.getContext("webgl2", { alpha: false, antialias: true, preserveDrawingBuffer: true });
    if (!gl) return; // still nothing we can do; stays blank until the module remounts us
    this.gl = gl;
    this.initGlResources();
    this.uploadToGpu();
    this.handleResize();
    this.scheduleDraw();
  };

  mount(el: HTMLElement, layout: Layout): void {
    this.teardownDom();
    this.el = el;
    this.layout = layout;

    const wrapper = document.createElement("div");
    wrapper.className = "relative h-full w-full";
    const glCanvas = document.createElement("canvas");
    glCanvas.className = "absolute inset-0 block h-full w-full touch-none";
    glCanvas.dataset.role = "webgl2-gl"; // e2e hook (web/e2e/ui-smoke.js) — distinguishes it from the overlay canvas
    const overlayCanvas = document.createElement("canvas");
    overlayCanvas.className = "pointer-events-none absolute inset-0 block h-full w-full";
    overlayCanvas.dataset.role = "webgl2-overlay";
    wrapper.append(glCanvas, overlayCanvas);
    el.replaceChildren(wrapper);
    this.glCanvas = glCanvas;
    this.overlayCanvas = overlayCanvas;

    const gl = glCanvas.getContext("webgl2", { alpha: false, antialias: true, preserveDrawingBuffer: true });
    if (!gl) {
      // Leave the (now-empty) canvases in place; the graph module is
      // expected to catch this and fall back to another renderer entirely.
      throw new RendererUnavailableError(this.id);
    }
    this.gl = gl;
    this.overlayCtx = overlayCanvas.getContext("2d");
    this.initGlResources();

    glCanvas.addEventListener("wheel", this.handleWheel, { passive: false });
    glCanvas.addEventListener("pointerdown", this.handlePointerDown);
    glCanvas.addEventListener("pointermove", this.handlePointerMove);
    glCanvas.addEventListener("pointerup", this.handlePointerUp);
    glCanvas.addEventListener("pointercancel", this.handlePointerUp);
    glCanvas.addEventListener("dblclick", this.handleDoubleClick);
    glCanvas.addEventListener("webglcontextlost", this.handleContextLost, false);
    glCanvas.addEventListener("webglcontextrestored", this.handleContextRestored, false);

    // The container's own CSS gives it a definite height (SPEC §11.3 — this
    // renderer never derives it from content), so observing it is enough to
    // catch every resize, DPR change included via the RAF-coalesced redraw.
    this.resizeObserver = new ResizeObserver(() => this.handleResize());
    this.resizeObserver.observe(el);
    this.handleResize();

    this.buildCpuGeometry();
    this.uploadToGpu();
    this.scheduleDraw();
  }

  /**
   * A new panel layout for the plot already mounted: the lanes' ranges,
   * offsets or membership changed, the data did not.
   *
   * The graph module used to re-`mount()` for this, which tears down the
   * canvases and takes a fresh WebGL context every time a range or offset
   * changed - a context per keystroke, against a browser limit of a handful
   * of live ones (B-55). Rebuilding the geometry against the new layout is
   * all that is needed.
   */
  setLayout(layout: Layout): void {
    this.layout = layout;
    this.buildCpuGeometry();
    this.uploadToGpu();
    this.scheduleDraw();
  }

  setTraces(traces: TraceSet): void {
    this.traces = traces;
    this.buildCpuGeometry();
    this.uploadToGpu();
    if (!this.viewport) {
      const bounds = traceSetTimeBounds(traces.traces);
      if (bounds) {
        this.viewport = fitAllViewport(bounds);
        // The graph module has no other way to learn the range this
        // renderer picked for the very first frame of a capture.
        this.notifyViewportChange();
      }
    }
    this.scheduleDraw();
  }

  setAnnotations(lanes: readonly AnnotationLane[]): void {
    this.annotationLanes = lanes;
    this.scheduleDraw();
  }

  setViewport(t0: number, t1: number): void {
    this.viewport = { t0, t1 };
    this.scheduleDraw();
  }

  onViewportChange(cb: (t0: number, t1: number) => void): () => void {
    this.listeners.add(cb);
    return () => this.listeners.delete(cb);
  }

  setTheme(colors: ThemeColors): void {
    this.theme = colors;
    // The channel palette is theme-aware, and its choice is
    // baked into `pending`/`drawables` at build time — so a theme switch
    // with no new traces (e.g. flipping the switch mid-capture) needs its
    // own rebuild, or channel colors would only catch up once fresh data
    // triggered the next `setTraces()`.
    this.buildCpuGeometry();
    this.uploadToGpu();
    this.scheduleDraw();
  }

  destroy(): void {
    this.resizeObserver?.disconnect();
    this.resizeObserver = null;
    if (this.rafHandle !== null) {
      cancelAnimationFrame(this.rafHandle);
      this.rafHandle = null;
    }
    this.teardownDom();
    this.layout = { panels: [] };
    this.traces = { traces: [] };
    this.annotationLanes = [];
    this.pending = [];
    this.viewport = null;
    this.theme = null;
    this.dragState = null;
    this.listeners.clear();
  }

  private teardownDom(): void {
    if (this.glCanvas) {
      this.glCanvas.removeEventListener("wheel", this.handleWheel);
      this.glCanvas.removeEventListener("pointerdown", this.handlePointerDown);
      this.glCanvas.removeEventListener("pointermove", this.handlePointerMove);
      this.glCanvas.removeEventListener("pointerup", this.handlePointerUp);
      this.glCanvas.removeEventListener("pointercancel", this.handlePointerUp);
      this.glCanvas.removeEventListener("dblclick", this.handleDoubleClick);
      this.glCanvas.removeEventListener("webglcontextlost", this.handleContextLost);
      this.glCanvas.removeEventListener("webglcontextrestored", this.handleContextRestored);
    }
    this.deleteDrawables();
    if (this.gl && this.program) this.gl.deleteProgram(this.program);
    this.program = null;
    this.uniforms = null;
    this.gl = null;
    this.overlayCtx = null;
    this.el?.replaceChildren();
    this.el = null;
    this.glCanvas = null;
    this.overlayCanvas = null;
  }

  // ---------------------------------------------------------------------
  // Geometry: CPU build (survives context loss) + GPU upload
  // ---------------------------------------------------------------------

  /** The panel key format `plotly-mapping.ts`'s (unexported) `panelKey` uses — kept in sync deliberately, not re-exported, since it's an internal map key, not part of either module's public contract. */
  private static panelKeyFor(trace: Trace): string {
    const kind = trace.kind === "analog-envelope" ? "analog" : trace.kind;
    return `${kind}:${trace.channel}`;
  }

  private buildCpuGeometry(): void {
    const traces = this.traces.traces;
    const bounds = traceSetTimeBounds(traces);
    this.originSeconds = bounds?.t0 ?? 0;
    const index = panelIndex(this.layout);
    const items: PendingItem[] = [];
    // The channel palette is theme-aware: which shade of a
    // channel's hue clears 3:1 contrast depends on the plot background, so
    // every `channelColor` call below needs it. Falls back to the light
    // background before the first `setTheme()` call, matching the other
    // `this.theme?.___ ?? ...` fallbacks in this file.
    const background = this.theme?.background ?? "#ffffff";

    for (const trace of traces) {
      const panel = index.get(Webgl2Renderer.panelKeyFor(trace));
      if (panel === undefined) continue; // traces momentarily ahead of a layout change; draw nothing rather than throw
      const spec = this.layout.panels[panel]!;
      // SPEC §5.2: a range setting is a ± full-scale range, so the lane is
      // twice it (B-55).
      const range = spec.kind === "analog" ? analogPanelSpan(spec.rangeV) : DIGITAL_RANGE;
      const offset = spec.kind === "analog" ? spec.offsetV : DIGITAL_OFFSET;

      switch (trace.kind) {
        case "analog":
          items.push({
            data: buildAnalogLineVertices(trace, this.originSeconds),
            primitive: GL_LINE_STRIP,
            panel,
            range,
            offset,
            color: channelColor(trace.channel, background),
          });
          break;
        case "analog-envelope": {
          items.push({
            data: buildAnalogEnvelopeVertices(trace, this.originSeconds),
            primitive: GL_TRIANGLE_STRIP,
            panel,
            range,
            offset,
            color: channelColor(trace.channel, background, 0.35),
          });
          // The outline: a solid stroke around the band so it
          // reads as one continuous shape even where the semi-transparent
          // fill alone is fragile to sub-pixel-wide buckets.
          const outline = buildAnalogEnvelopeOutlineVertices(trace, this.originSeconds);
          const strokeColor = channelColor(trace.channel, background, 0.9);
          if (outline.top.length > 0) {
            items.push({ data: outline.top, primitive: GL_LINE_STRIP, panel, range, offset, color: strokeColor });
          }
          if (outline.bottom.length > 0) {
            items.push({ data: outline.bottom, primitive: GL_LINE_STRIP, panel, range, offset, color: strokeColor });
          }
          break;
        }
        case "digital": {
          const level = buildDigitalLevelVertices(trace, this.originSeconds);
          if (level.length > 0) {
            items.push({ data: level, primitive: GL_LINES, panel, range, offset, color: channelColor(trace.channel, background) });
          }
          const toggle = buildDigitalToggleVertices(trace, this.originSeconds);
          if (toggle.length > 0) {
            items.push({
              data: toggle,
              primitive: GL_TRIANGLES,
              panel,
              range,
              offset,
              color: channelColor(trace.channel, background, 0.55),
            });
          }
          break;
        }
      }
    }
    this.pending = items;
  }

  private uploadToGpu(): void {
    this.deleteDrawables();
    const gl = this.gl;
    if (!gl) return; // context lost/unavailable — `pending` stays ready for the next restore
    this.drawables = this.pending.map((item) => {
      const buffer = gl.createBuffer();
      if (!buffer) throw new Error("webgl2 renderer: failed to allocate a vertex buffer");
      gl.bindBuffer(gl.ARRAY_BUFFER, buffer);
      gl.bufferData(gl.ARRAY_BUFFER, item.data, gl.STATIC_DRAW);
      return {
        buffer,
        vertexCount: item.data.length / 2,
        primitive: item.primitive,
        panel: item.panel,
        range: item.range,
        offset: item.offset,
        color: item.color,
      };
    });
  }

  private deleteDrawables(): void {
    if (this.gl) for (const d of this.drawables) this.gl.deleteBuffer(d.buffer);
    this.drawables = [];
  }

  private initGlResources(): void {
    const gl = this.gl;
    if (!gl) return;
    const program = linkProgram(gl);
    this.program = program;
    this.uniforms = {
      xScale: gl.getUniformLocation(program, "u_xScale")!,
      xOffset: gl.getUniformLocation(program, "u_xOffset")!,
      yScale: gl.getUniformLocation(program, "u_yScale")!,
      yOffset: gl.getUniformLocation(program, "u_yOffset")!,
      color: gl.getUniformLocation(program, "u_color")!,
    };
  }

  // ---------------------------------------------------------------------
  // Sizing (SPEC §11.3: never derive the container's height from content)
  // ---------------------------------------------------------------------

  private handleResize(): void {
    if (!this.el || !this.glCanvas || !this.overlayCanvas) return;
    const rect = this.el.getBoundingClientRect();
    const cssW = Math.max(1, Math.round(rect.width));
    const cssH = Math.max(1, Math.round(rect.height));
    const dpr = window.devicePixelRatio || 1;
    const pxW = Math.max(1, Math.round(cssW * dpr));
    const pxH = Math.max(1, Math.round(cssH * dpr));

    for (const canvas of [this.glCanvas, this.overlayCanvas]) {
      if (canvas.width !== pxW || canvas.height !== pxH) {
        canvas.width = pxW;
        canvas.height = pxH;
      }
    }
    this.gl?.viewport(0, 0, pxW, pxH);
    this.overlayCtx?.setTransform(dpr, 0, 0, dpr, 0, 0);
    this.scheduleDraw();
  }

  // ---------------------------------------------------------------------
  // Drawing — coalesced to one requestAnimationFrame per change (SPEC §5.3)
  // ---------------------------------------------------------------------

  private notifyViewportChange(): void {
    if (!this.viewport) return;
    for (const cb of this.listeners) cb(this.viewport.t0, this.viewport.t1);
  }

  private scheduleDraw(): void {
    if (this.rafHandle !== null) return;
    this.rafHandle = requestAnimationFrame(() => {
      this.rafHandle = null;
      this.draw();
    });
  }

  private draw(): void {
    this.drawGl();
    this.drawOverlay();
  }

  private drawGl(): void {
    const gl = this.gl;
    const program = this.program;
    const uniforms = this.uniforms;
    if (!gl || !program || !uniforms) return;

    const [r, g, b] = this.theme ? parseHexColor(this.theme.background) : [1, 1, 1];
    gl.clearColor(r, g, b, 1);
    gl.clear(gl.COLOR_BUFFER_BIT);
    if (!this.viewport || this.drawables.length === 0) return;

    gl.useProgram(program);
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);
    gl.enableVertexAttribArray(0);

    const panelCount = Math.max(this.layout.panels.length, 1);
    const timeT = timeAxisTransform(this.viewport.t0, this.viewport.t1, this.originSeconds);
    gl.uniform1f(uniforms.xScale, timeT.scale);
    gl.uniform1f(uniforms.xOffset, timeT.offset);

    // Each lane is drawn inside its own scissor box: GL otherwise clips only
    // at the edge of the drawing buffer, and a signal above its lane was drawn
    // straight through the lane above it (B-55).
    const pxW = this.glCanvas?.width ?? 0;
    const pxH = this.glCanvas?.height ?? 0;
    gl.enable(gl.SCISSOR_TEST);
    for (const d of this.drawables) {
      const row = panelRowClip(d.panel, panelCount);
      const valueT = valueAxisTransform(row.top, row.bottom, d.range, d.offset);
      const box = panelScissorRect(d.panel, panelCount, pxW, pxH);
      gl.scissor(box.x, box.y, box.width, box.height);
      gl.uniform1f(uniforms.yScale, valueT.scale);
      gl.uniform1f(uniforms.yOffset, valueT.offset);
      gl.uniform4f(uniforms.color, d.color[0], d.color[1], d.color[2], d.color[3]);
      gl.bindBuffer(gl.ARRAY_BUFFER, d.buffer);
      gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 0, 0);
      gl.drawArrays(d.primitive, 0, d.vertexCount);
    }
    gl.disable(gl.SCISSOR_TEST);
  }

  private drawOverlay(): void {
    const ctx = this.overlayCtx;
    if (!ctx || !this.el || !this.viewport) return;
    const rect = this.el.getBoundingClientRect();
    const cssW = rect.width;
    const cssH = rect.height;
    ctx.clearRect(0, 0, cssW, cssH);

    const gridColor = this.theme?.border ?? "#d8d8dc";
    const textColor = this.theme?.mutedForeground ?? "#5b5d63";
    ctx.font = "11px system-ui, sans-serif";
    ctx.textBaseline = "alphabetic";

    const targetTicks = Math.max(3, Math.floor(cssW / 90));
    const ticks = timeTicks(this.viewport.t0, this.viewport.t1, targetTicks);
    const stepSeconds = ticks.length > 1 ? ticks[1]! - ticks[0]! : this.viewport.t1 - this.viewport.t0;

    ctx.strokeStyle = gridColor;
    ctx.fillStyle = textColor;
    ctx.lineWidth = 1;
    for (const t of ticks) {
      const x = timeToPointerX(t, cssW, this.viewport);
      ctx.beginPath();
      ctx.moveTo(x + 0.5, 0);
      ctx.lineTo(x + 0.5, cssH);
      ctx.stroke();
      ctx.fillText(formatSiTime(t, stepSeconds), Math.min(Math.max(x + 3, 2), Math.max(cssW - 44, 2)), cssH - 4);
    }

    const panelCount = Math.max(this.layout.panels.length, 1);
    this.layout.panels.forEach((panel, i) => {
      const row = panelRowClip(i, panelCount);
      const topCss = ((1 - row.top) / 2) * cssH;
      const bottomCss = ((1 - row.bottom) / 2) * cssH;

      ctx.strokeStyle = gridColor;
      ctx.beginPath();
      ctx.moveTo(0, bottomCss + 0.5);
      ctx.lineTo(cssW, bottomCss + 0.5);
      ctx.stroke();

      ctx.fillStyle = textColor;
      ctx.fillText(panel.label, 4, topCss + 12);

      if (panel.kind === "analog") {
        const { min: vMin, max: vMax } = analogPanelBounds(panel.rangeV, panel.offsetV);
        const vTicks = voltTicks(vMin, vMax, 3);
        const vStep = vTicks.length > 1 ? vTicks[1]! - vTicks[0]! : panel.rangeV;
        const valueT = valueAxisTransform(row.top, row.bottom, analogPanelSpan(panel.rangeV), panel.offsetV);
        for (const v of vTicks) {
          const clipY = v * valueT.scale + valueT.offset;
          const yCss = ((1 - clipY) / 2) * cssH;
          if (yCss < topCss + 10 || yCss > bottomCss - 2) continue; // keep volt labels off the channel-name/grid lines
          ctx.fillText(formatVolts(v, vStep), 4, yCss - 2);
        }
      } else {
        this.drawAnnotationLane(ctx, panel.channel, topCss, bottomCss, cssW);
      }
    });
  }

  /**
   * Decoded annotations for one digital channel, drawn as boxes "under the
   * traces" (SPEC §11.3): the lane's own bottom fraction of the panel row,
   * so the level line itself stays fully visible above it. Frame (level 0)
   * occupies the row nearest the trace, field and bit below it — coarser
   * boxes read first, finer detail only shows once there is room to draw it
   * (SPEC T21: "fields and bits as finer boxes when zoomed in").
   */
  private drawAnnotationLane(
    ctx: CanvasRenderingContext2D,
    channel: number,
    topCss: number,
    bottomCss: number,
    cssW: number,
  ): void {
    if (!this.viewport) return;
    const lane = this.annotationLanes.find((l) => l.channel === channel);
    if (!lane || lane.boxes.length === 0) return;

    const laneTop = topCss + (bottomCss - topCss) * 0.55;
    const laneBottom = bottomCss - 1;
    const laid = layoutAnnotations(lane.boxes, { widthPx: cssW, viewport: this.viewport });
    const background = this.theme?.background ?? "#ffffff";
    const border = this.theme?.border ?? "#d8d8dc";
    const foreground = this.theme?.foreground ?? "#18181b";

    for (const box of laid) {
      const { top, bottom } = levelRowBand(box.level, laneTop, laneBottom);
      const w = Math.max(box.x1 - box.x0, 1);
      ctx.fillStyle = background;
      ctx.globalAlpha = 0.85;
      ctx.fillRect(box.x0, top, w, Math.max(bottom - top - 1, 1));
      ctx.globalAlpha = 1;
      ctx.strokeStyle = border;
      ctx.lineWidth = 1;
      ctx.strokeRect(box.x0 + 0.5, top + 0.5, Math.max(w - 1, 0), Math.max(bottom - top - 2, 0));
      if (box.showLabel) {
        ctx.fillStyle = foreground;
        ctx.save();
        ctx.beginPath();
        ctx.rect(box.x0, top, w, bottom - top);
        ctx.clip();
        ctx.fillText(box.label, box.x0 + 2, bottom - 3);
        ctx.restore();
      }
    }
  }
}

registerRenderer("webgl2", () => new Webgl2Renderer());
