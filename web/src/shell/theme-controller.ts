import { Emitter } from "../ws/emitter";
import { applyResolvedTheme, loadThemeChoice, resolveTheme, saveThemeChoice, type ResolvedTheme, type ThemeChoice, type ThemeRoot, type ThemeStorage } from "./theme";

/** The slice of `MediaQueryList` this needs — `window.matchMedia(...)` satisfies it structurally. */
export interface MediaQueryLike {
  readonly matches: boolean;
  addEventListener(type: "change", listener: () => void): void;
  removeEventListener(type: "change", listener: () => void): void;
}

export interface ThemeControllerOptions {
  /** `null` to force "no storage available"; omit to use real `localStorage`. */
  readonly storage?: ThemeStorage | null;
  /** Omit to use `document.documentElement`. */
  readonly root?: ThemeRoot;
  /** Omit to use `window.matchMedia("(prefers-color-scheme: dark)")`. */
  readonly media?: MediaQueryLike;
}

/**
 * Ties the pure theme functions (`theme.ts`) to the document and the OS
 * preference (SPEC §11.1). Loads the persisted choice, applies it, and keeps
 * `resolved` current — including while `choice` is `"system"`: it tracks
 * live `prefers-color-scheme` changes so a `changes` subscriber (the Plotly
 * renderer, via the graph module) can re-color a plot even if the user never
 * touches the app's own switch, only their OS setting.
 *
 * `applyResolvedTheme` always sets `data-theme` to the *resolved* value
 * (`"light"` or `"dark"`, never absent) — the CSS in `styles/index.css` does
 * the actual recoloring of every `--color-awic-*` custom property from that
 * attribute. This controller's `changes` emitter exists purely for the one
 * thing CSS cannot reach into on its own: Plotly's SVG, which needs its
 * colors set explicitly and re-set on every change (SPEC §11.1).
 */
export class ThemeController {
  readonly changes = new Emitter<ResolvedTheme>();

  private readonly storage: ThemeStorage | null | undefined;
  private readonly root: ThemeRoot;
  private readonly media: MediaQueryLike;
  private choiceValue: ThemeChoice;

  private readonly onMediaChange = (): void => {
    if (this.choiceValue === "system") this.applyAndNotify();
  };

  constructor(options: ThemeControllerOptions = {}) {
    this.storage = options.storage;
    this.root = options.root ?? document.documentElement;
    this.media = options.media ?? window.matchMedia("(prefers-color-scheme: dark)");
    this.choiceValue = loadThemeChoice(this.storage);
    this.media.addEventListener("change", this.onMediaChange);
    this.applyAndNotify();
  }

  get choice(): ThemeChoice {
    return this.choiceValue;
  }

  get resolved(): ResolvedTheme {
    return resolveTheme(this.choiceValue, this.media.matches);
  }

  setChoice(choice: ThemeChoice): void {
    if (choice === this.choiceValue) return;
    this.choiceValue = choice;
    saveThemeChoice(choice, this.storage);
    this.applyAndNotify();
  }

  /** Stops tracking OS-preference changes. Call when the controller itself is torn down (it never is, today — one per app). */
  destroy(): void {
    this.media.removeEventListener("change", this.onMediaChange);
  }

  private applyAndNotify(): void {
    applyResolvedTheme(this.resolved, this.root);
    this.changes.emit(this.resolved);
  }
}
