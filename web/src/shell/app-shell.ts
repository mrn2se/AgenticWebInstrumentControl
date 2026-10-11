import type { ModuleContext } from "./context";
import { listModules } from "./registry";
import type { AwicModule } from "./module";
import { type ErrorEntry, dismissError } from "../store/state";
import type { ConnectionState } from "../ws/client";
import type { ThemeChoice } from "./theme";
import { activationRequests } from "./activation";

const STATUS_LABEL: Record<ConnectionState, string> = {
  connecting: "Connecting…",
  connected: "Connected",
  reconnecting: "Reconnecting…",
  offline: "Offline",
};

const STATUS_DOT_CLASS: Record<ConnectionState, string> = {
  connected: "bg-awic-ok",
  connecting: "bg-awic-accent",
  reconnecting: "bg-awic-accent",
  offline: "bg-awic-danger",
};

// A colorless base plus exactly one color variant: when two classes on one
// element set the same property (say `text-awic-fg-muted` and
// `text-awic-accent-fg`), Tailwind's compiled rule order decides, not the
// order in the class attribute — which left the active tab's label
// unreadable (muted text on the accent background).
const TAB_BASE = "rounded-md border px-3 py-1.5 text-sm font-medium transition-colors";
const TAB_INACTIVE = "border-transparent text-awic-fg-muted hover:bg-awic-bg hover:text-awic-fg";
const TAB_ACTIVE = "border-awic-accent bg-awic-accent text-awic-accent-fg";

const THEME_BTN_BASE = "rounded-md border px-2 py-1 text-xs font-medium transition-colors";
const THEME_BTN_INACTIVE = "border-awic-border bg-awic-bg-raised text-awic-fg-muted hover:bg-awic-bg";
const THEME_BTN_ACTIVE = "border-awic-accent bg-awic-accent text-awic-accent-fg";

/**
 * The application shell: tab navigation over registered modules, a
 * connection-state indicator, and a visible error surface for
 * `{ ok: false, error }` responses and other client-surfaced errors
 * (SPECIFICATION.md §11). Light DOM (SPEC §11, see `shell/module.ts`) — its
 * own markup and the modules it mounts all share the one Tailwind
 * stylesheet.
 */
export class AppShell extends HTMLElement {
  // Assigned in `build()`, not the constructor: the Custom Elements spec
  // forbids a constructor from leaving the element with any attributes or
  // children once `document.createElement` returns — exactly what setting
  // `className`/`innerHTML` on `this` would do here (this is *why* the
  // original shadow-DOM version could build in its constructor: shadow-root
  // content isn't "this element's children" at all). `!` asserts they are
  // definitely assigned by the time anything else runs, since `build()` is
  // always the first thing `setContext()` does.
  private tabsEl!: HTMLElement;
  private contentEl!: HTMLElement;
  private statusEl!: HTMLElement;
  private statusDotEl!: HTMLElement;
  private statusLabelEl!: HTMLElement;
  private errorsEl!: HTMLElement;
  private themeSwitchEl!: HTMLElement;

  private ctx: ModuleContext | null = null;
  private readonly mounted = new Map<string, AwicModule>();
  private activeId: string | null = null;
  private readonly unsubscribers: Array<() => void> = [];

  constructor() {
    super();
  }

  /** Builds the shell's own light DOM. See the field comments above for why this cannot run in the constructor. */
  private build(): void {
    this.className = "flex min-h-0 flex-1 flex-col";
    this.innerHTML = `
      <header class="flex items-center gap-4 border-b border-awic-border bg-awic-bg-raised px-4 py-2">
        <span class="font-semibold tracking-wide">AWIC</span>
        <nav id="tabs" class="flex flex-1 gap-1 overflow-x-auto"></nav>
        <span id="status" class="flex items-center gap-1.5 whitespace-nowrap text-sm text-awic-fg-muted">
          <span id="status-dot" class="h-2.5 w-2.5 rounded-full bg-awic-fg-muted"></span>
          <span id="status-label"></span>
        </span>
        <div id="theme-switch" class="flex gap-1"></div>
      </header>
      <div id="errors" class="flex flex-col gap-1 px-4 py-2 empty:hidden"></div>
      <main id="content" class="flex min-h-0 flex-1 flex-col"></main>
    `;
    this.tabsEl = this.requireEl("#tabs");
    this.contentEl = this.requireEl("#content");
    this.statusEl = this.requireEl("#status");
    this.statusDotEl = this.requireEl("#status-dot");
    this.statusLabelEl = this.requireEl("#status-label");
    this.errorsEl = this.requireEl("#errors");
    this.themeSwitchEl = this.requireEl("#theme-switch");
  }

  /**
   * Wires the shell to the shared context and renders the first tab. Split
   * out from the constructor because custom elements must have zero-arg
   * constructors — and, in light DOM, must not populate their own content
   * until after construction (see the field comments above); call this once,
   * before appending the element to the DOM.
   */
  setContext(ctx: ModuleContext): void {
    this.build();
    this.ctx = ctx;
    this.renderTabs();
    this.unsubscribers.push(ctx.store.subscribe((state) => this.renderStatus(state.connection)));
    this.unsubscribers.push(ctx.store.subscribe((state) => this.renderErrors(state.errors)));
    this.renderThemeSwitch(ctx);
    // Cross-module "jump to this tab, and here's a hint" — the USB tree's
    // terminal icon switches to the Terminal tab and opens a specific
    // source (shell/activation.ts).
    this.unsubscribers.push(activationRequests.on((req) => this.activateTab(req.moduleId, req.detail)));

    const first = listModules()[0];
    if (first) this.activateTab(first.id);
  }

  disconnectedCallback(): void {
    for (const unsubscribe of this.unsubscribers.splice(0)) unsubscribe();
  }

  private requireEl<T extends HTMLElement = HTMLElement>(selector: string): T {
    const el = this.querySelector<T>(selector);
    if (!el) throw new Error(`app-shell: missing ${selector}`);
    return el;
  }

  private renderTabs(): void {
    this.tabsEl.replaceChildren();
    const modules = listModules();
    if (modules.length === 0) return;
    for (const descriptor of modules) {
      const button = document.createElement("button");
      button.type = "button";
      button.className = `${TAB_BASE} ${TAB_INACTIVE}`;
      button.textContent = descriptor.title;
      button.dataset.moduleId = descriptor.id;
      button.addEventListener("click", () => this.activateTab(descriptor.id));
      this.tabsEl.appendChild(button);
    }
  }

  private activateTab(id: string, detail?: unknown): void {
    if (!this.ctx) return;
    const descriptor = listModules().find((m) => m.id === id);
    if (!descriptor) return;

    if (this.activeId !== id) {
      const previous = this.activeId ? this.mounted.get(this.activeId) : undefined;
      if (previous) {
        previous.deactivate();
        previous.hidden = true;
      }

      let element = this.mounted.get(id);
      if (!element) {
        element = descriptor.factory();
        element.init(this.ctx);
        this.contentEl.appendChild(element);
        this.mounted.set(id, element);
      }
      element.hidden = false;
      element.activate();

      this.activeId = id;
      this.highlightTab(id);
    }

    // Delivered whether or not this tab was already active: re-requesting
    // activation with a new `detail` (e.g. a different USB port's terminal
    // icon) must still reach the module even if its tab was already open.
    if (detail !== undefined) {
      this.mounted.get(id)?.onActivationDetail(detail);
    }
  }

  private highlightTab(id: string): void {
    const buttons = this.tabsEl.querySelectorAll<HTMLButtonElement>("button");
    for (const button of buttons) {
      button.className = `${TAB_BASE} ${button.dataset.moduleId === id ? TAB_ACTIVE : TAB_INACTIVE}`;
    }
  }

  /** The light/dark switch (SPEC §11.1): system (default) / light / dark, persisted per browser. */
  private renderThemeSwitch(ctx: ModuleContext): void {
    this.themeSwitchEl.replaceChildren();
    const options: Array<{ choice: ThemeChoice; label: string }> = [
      { choice: "system", label: "System" },
      { choice: "light", label: "Light" },
      { choice: "dark", label: "Dark" },
    ];
    for (const { choice, label } of options) {
      const button = document.createElement("button");
      button.type = "button";
      button.textContent = label;
      const active = choice === ctx.theme.choice;
      button.className = `${THEME_BTN_BASE} ${active ? THEME_BTN_ACTIVE : THEME_BTN_INACTIVE}`;
      button.setAttribute("aria-pressed", String(active));
      button.addEventListener("click", () => {
        ctx.theme.setChoice(choice);
        this.renderThemeSwitch(ctx);
      });
      this.themeSwitchEl.appendChild(button);
    }
  }

  private renderStatus(connection: ConnectionState): void {
    this.statusLabelEl.textContent = STATUS_LABEL[connection];
    this.statusEl.dataset.state = connection;
    this.statusDotEl.className = `h-2.5 w-2.5 rounded-full ${STATUS_DOT_CLASS[connection]}`;
  }

  private renderErrors(errors: readonly ErrorEntry[]): void {
    this.errorsEl.replaceChildren();
    for (const err of errors.slice(-5)) {
      const row = document.createElement("div");
      row.className = "flex items-center justify-between gap-3 rounded-md bg-awic-danger-bg px-2.5 py-1.5 text-sm text-awic-danger";

      const text = document.createElement("span");
      text.textContent = `[${err.code}] ${err.msg}`;

      const dismiss = document.createElement("button");
      dismiss.type = "button";
      dismiss.className = "shrink-0 text-base leading-none text-current hover:opacity-70";
      dismiss.textContent = "×";
      dismiss.setAttribute("aria-label", "Dismiss");
      dismiss.addEventListener("click", () => {
        this.ctx?.store.update((state) => dismissError(state, err.seq));
      });

      row.append(text, dismiss);
      this.errorsEl.appendChild(row);
    }
  }
}

customElements.define("awic-app", AppShell);
