import type { ModuleContext } from "./context";

/**
 * Base web-component class every module (the USB tree, the graph module)
 * extends. Renders into its own **light DOM** — no shadow root. Tailwind
 * produces one global stylesheet (SPECIFICATION.md §11), and a global
 * stylesheet cannot reach inside a shadow root at all, so the shell
 * (`app-shell.ts`) and every module share the page's light DOM instead of
 * each carrying its own isolated `<style>` block.
 *
 * Lifecycle, driven by the shell (`app-shell.ts`), not by DOM
 * connect/disconnect: `init()` runs once, right after the shell creates the
 * element and before it is ever shown; `activate()`/`deactivate()` run every
 * time the shell's tab selection moves to or away from this module. Modules
 * stay attached to the DOM (hidden, not removed) between activations so
 * they don't lose scroll position or in-progress state — see `app-shell.ts`.
 *
 * Inactive modules carry the boolean `hidden` attribute. Tailwind's
 * preflight declares `[hidden] { display: none !important }`, which is
 * `!important` specifically so it always wins over a module's own display
 * utility regardless of specificity or source order — a module must not
 * fight it with an `!`-important display utility of its own. An earlier
 * shadow-DOM version of this bug had a module's `:host { display: ... }`
 * outrank `[hidden]` and stack every module on screen at once; light DOM
 * plus Tailwind's `!important` preflight rule is what keeps that fixed (see
 * `e2e/ui-smoke.js`, check 1).
 */
export abstract class AwicModule extends HTMLElement {
  private context: ModuleContext | null = null;

  /**
   * Public (not `protected`) even though the class is `abstract` and so
   * cannot be instantiated directly: `customElements.define` requires a
   * concrete, public, zero-arg constructor type, and concrete subclasses
   * that declare no constructor of their own inherit this one as-is.
   */
  constructor() {
    super();
  }

  /** Called once by the shell immediately after construction. */
  init(context: ModuleContext): void {
    this.context = context;
    this.onInit();
  }

  protected get ctx(): ModuleContext {
    if (!this.context) {
      throw new Error(`${this.tagName.toLowerCase()}: accessed ctx before init()`);
    }
    return this.context;
  }

  /** Override to do setup that needs `ctx` (subscriptions, initial requests). */
  protected onInit(): void {}

  /** Called by the shell when this module's tab becomes the active one. */
  activate(): void {}

  /** Called by the shell when the user navigates to a different tab. */
  deactivate(): void {}

  /**
   * Called by the shell right after `activate()` when this module's tab was
   * switched to via `requestModuleActivation(id, detail)` (`shell/activation.ts`)
   * rather than a plain tab click — e.g. the USB tree's terminal icon
   * switching to the Terminal tab with the source id to open. Default
   * no-op; a module that cares overrides it.
   */
  onActivationDetail(_detail: unknown): void {}

  /**
   * Finds a required descendant within this module's own light DOM. Throws
   * rather than returning `null | undefined` — a missing element here is a
   * wiring bug in the module's own template, not a runtime condition any
   * caller could sensibly handle.
   */
  protected requireEl<T extends HTMLElement = HTMLElement>(selector: string): T {
    const found = this.querySelector<T>(selector);
    if (!found) throw new Error(`${this.tagName.toLowerCase()}: missing ${selector}`);
    return found;
  }
}
