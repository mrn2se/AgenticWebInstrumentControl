import { describe, expect, it } from "vitest";
import { ThemeController, type MediaQueryLike } from "./theme-controller";
import type { ThemeRoot, ThemeStorage } from "./theme";

class FakeStorage implements ThemeStorage {
  private readonly map = new Map<string, string>();
  getItem(key: string): string | null {
    return this.map.get(key) ?? null;
  }
  setItem(key: string, value: string): void {
    this.map.set(key, value);
  }
}

function fakeRoot(): ThemeRoot & { calls: string[] } {
  const calls: string[] = [];
  return {
    calls,
    setAttribute: (n: string, v: string) => calls.push(`set ${n}=${v}`),
    removeAttribute: (n: string) => calls.push(`remove ${n}`),
  };
}

class FakeMedia implements MediaQueryLike {
  matches: boolean;
  private listener: (() => void) | null = null;
  constructor(matches: boolean) {
    this.matches = matches;
  }
  addEventListener(_type: "change", listener: () => void): void {
    this.listener = listener;
  }
  removeEventListener(_type: "change", listener: () => void): void {
    if (this.listener === listener) this.listener = null;
  }
  /** Test helper: simulates the OS/browser preference flipping. */
  fireChange(matches: boolean): void {
    this.matches = matches;
    this.listener?.();
  }
}

describe("ThemeController", () => {
  it("loads the persisted choice on construction and applies it immediately", () => {
    const storage = new FakeStorage();
    storage.setItem("awic-theme", "dark");
    const root = fakeRoot();
    const controller = new ThemeController({ storage, root, media: new FakeMedia(false) });

    expect(controller.choice).toBe("dark");
    expect(controller.resolved).toBe("dark");
    expect(root.calls).toEqual(["set data-theme=dark"]);
  });

  it("defaults to system when nothing was persisted, resolved from the media query", () => {
    const root = fakeRoot();
    const controller = new ThemeController({ storage: new FakeStorage(), root, media: new FakeMedia(true) });
    expect(controller.choice).toBe("system");
    expect(controller.resolved).toBe("dark");
    // The choice stays "system" (untouched in storage), but the DOM attribute
    // always carries the *resolved* value — never absent — so plain CSS
    // selectors and Tailwind's `@custom-variant dark` never need to also
    // understand "system".
    expect(root.calls).toEqual(["set data-theme=dark"]);
  });

  it("setChoice persists, re-applies data-theme, and emits the newly resolved theme", () => {
    const storage = new FakeStorage();
    const root = fakeRoot();
    const controller = new ThemeController({ storage, root, media: new FakeMedia(false) });

    const seen: string[] = [];
    controller.changes.on((theme) => seen.push(theme));

    controller.setChoice("dark");

    expect(controller.choice).toBe("dark");
    expect(storage.getItem("awic-theme")).toBe("dark");
    expect(root.calls).toContain("set data-theme=dark");
    expect(seen).toEqual(["dark"]);
  });

  it("does nothing on setChoice with the same choice already active (no redundant emit)", () => {
    const controller = new ThemeController({ storage: new FakeStorage(), root: fakeRoot(), media: new FakeMedia(false) });
    const seen: string[] = [];
    controller.changes.on((theme) => seen.push(theme));
    controller.setChoice("system");
    expect(seen).toEqual([]);
  });

  it("while system, tracks a live OS-preference change, re-applies data-theme, and emits the newly resolved theme", () => {
    const media = new FakeMedia(false);
    const root = fakeRoot();
    const controller = new ThemeController({ storage: new FakeStorage(), root, media });
    const seen: string[] = [];
    controller.changes.on((theme) => seen.push(theme));

    media.fireChange(true);

    expect(controller.resolved).toBe("dark");
    expect(seen).toEqual(["dark"]);
    expect(root.calls.at(-1)).toBe("set data-theme=dark");
  });

  it("ignores an OS-preference change once an explicit choice overrides it", () => {
    const media = new FakeMedia(false);
    const controller = new ThemeController({ storage: new FakeStorage(), root: fakeRoot(), media });
    controller.setChoice("light");

    const seen: string[] = [];
    controller.changes.on((theme) => seen.push(theme));
    media.fireChange(true);

    expect(controller.resolved).toBe("light");
    expect(seen).toEqual([]);
  });

  it("destroy() stops tracking OS-preference changes", () => {
    const media = new FakeMedia(false);
    const controller = new ThemeController({ storage: new FakeStorage(), root: fakeRoot(), media });
    controller.destroy();

    const seen: string[] = [];
    controller.changes.on((theme) => seen.push(theme));
    media.fireChange(true);

    expect(seen).toEqual([]);
  });
});
