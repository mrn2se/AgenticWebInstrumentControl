/**
 * Pure light/dark theme logic (SPECIFICATION.md §11.1): three states —
 * "system" (default, follows `prefers-color-scheme`), "light", "dark" —
 * persisted per browser. No DOM here beyond the small structural interfaces
 * below, so this is unit-testable in the plain Node vitest environment this
 * project uses (see `vite.config.ts`); `theme-controller.ts` is the thin
 * stateful layer that ties this to the real document, storage and OS
 * preference.
 */

export type ThemeChoice = "system" | "light" | "dark";
export type ResolvedTheme = "light" | "dark";

const STORAGE_KEY = "awic-theme";

/** The slice of the `Storage` interface this needs — real `localStorage` satisfies it structurally. */
export type ThemeStorage = Pick<Storage, "getItem" | "setItem">;

/** The slice of `HTMLElement` this needs — real `document.documentElement` satisfies it structurally. */
export type ThemeRoot = Pick<HTMLElement, "setAttribute" | "removeAttribute">;

function isThemeChoice(value: string | null): value is ThemeChoice {
  return value === "system" || value === "light" || value === "dark";
}

/** Reads `localStorage` directly; wrapped by every caller below in its own try/catch. */
function browserStorage(): ThemeStorage | null {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

/**
 * Reads the persisted theme choice. Storage can be unavailable — a private
 * window, disabled site data, a browser that blocks it entirely — so this is
 * wrapped in try/catch and falls back to `"system"` rather than throwing
 * into the caller; an unrecognized stored value falls back the same way.
 */
export function loadThemeChoice(storage: ThemeStorage | null = browserStorage()): ThemeChoice {
  try {
    const value = storage?.getItem(STORAGE_KEY) ?? null;
    return isThemeChoice(value) ? value : "system";
  } catch {
    return "system";
  }
}

/**
 * Persists a theme choice. Also wrapped in try/catch: a storage failure here
 * (quota, private mode, disabled) just means the choice won't survive a
 * reload — never an error the user needs to see.
 */
export function saveThemeChoice(choice: ThemeChoice, storage: ThemeStorage | null = browserStorage()): void {
  try {
    storage?.setItem(STORAGE_KEY, choice);
  } catch {
    // Unavailable storage is not fatal — see above.
  }
}

/** `"system"` follows the OS/browser preference; `"light"`/`"dark"` are explicit overrides. */
export function resolveTheme(choice: ThemeChoice, prefersDark: boolean): ResolvedTheme {
  if (choice === "system") return prefersDark ? "dark" : "light";
  return choice;
}

/**
 * Applies a *resolved* theme to the document root: `data-theme="light"` or
 * `data-theme="dark"`, always — never absent, even while the user's choice
 * is `"system"`. The Tailwind stylesheet (`styles/index.css`) redefines the
 * `--color-awic-*` color tokens under `[data-theme="dark"]` and declares
 * `@custom-variant dark` off the same attribute, so this is the single
 * switch that moves the whole page (and, via `plotly-theme.ts`, the plot).
 * `theme-controller.ts` is the only caller: it resolves `"system"` against
 * live `prefers-color-scheme` and calls this with the result, so the
 * attribute always reflects what should actually be on screen, not the raw
 * three-state choice (that is persisted separately, via `saveThemeChoice`).
 */
export function applyResolvedTheme(resolved: ResolvedTheme, root: ThemeRoot): void {
  root.setAttribute("data-theme", resolved);
}
