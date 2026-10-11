import { describe, expect, it } from "vitest";
import { createRenderer, listRenderers } from "./registry";
import { RendererNotImplementedError } from "./types";
import "./uplot";

/**
 * SPEC §5.3 / task instructions: `uplot` must be registered (a real seam a
 * config change can select) but must not half-render. This confirms it
 * reports a clear not-yet-implemented state instead of, say, silently
 * no-oping or drawing nothing. `webgl2` graduated out of this file once it
 * was implemented (see `webgl2.ts`); its own tests live alongside the pure
 * modules it's built from (`webgl2-geometry.test.ts`, etc.).
 */
describe("not-yet-implemented renderer backends", () => {
  it("registers itself under its own id", () => {
    expect(listRenderers()).toEqual(expect.arrayContaining(["uplot"]));
  });

  it("throws RendererNotImplementedError on mount()", () => {
    const renderer = createRenderer("uplot");
    const fakeEl = {} as HTMLElement;
    expect(() => renderer.mount(fakeEl, { panels: [] })).toThrow(RendererNotImplementedError);
    expect(() => renderer.mount(fakeEl, { panels: [] })).toThrow("uplot");
  });

  it("destroy() is always safe, even unmounted", () => {
    const renderer = createRenderer("uplot");
    expect(() => renderer.destroy()).not.toThrow();
  });

  it("setTheme() is a safe no-op, so a theme switch never crashes an unimplemented backend (SPEC §11.1)", () => {
    const renderer = createRenderer("uplot");
    expect(() =>
      renderer.setTheme({ background: "#000", foreground: "#fff", mutedForeground: "#aaa", border: "#222" }),
    ).not.toThrow();
  });

  it("onViewportChange() is a safe no-op that still returns a working unsubscribe function", () => {
    const renderer = createRenderer("uplot");
    let unsubscribe: (() => void) | null = null;
    expect(() => {
      unsubscribe = renderer.onViewportChange(() => {
        throw new Error("must never be called — nothing is ever mounted");
      });
    }).not.toThrow();
    expect(() => unsubscribe?.()).not.toThrow();
  });
});
