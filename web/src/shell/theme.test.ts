import { describe, expect, it } from "vitest";
import { applyResolvedTheme, loadThemeChoice, resolveTheme, saveThemeChoice, type ThemeStorage } from "./theme";

class FakeStorage implements ThemeStorage {
  private readonly map = new Map<string, string>();
  getItem(key: string): string | null {
    return this.map.get(key) ?? null;
  }
  setItem(key: string, value: string): void {
    this.map.set(key, value);
  }
}

class ThrowingStorage implements ThemeStorage {
  getItem(): string {
    throw new Error("storage unavailable");
  }
  setItem(): void {
    throw new Error("storage unavailable");
  }
}

describe("loadThemeChoice", () => {
  it("defaults to system with no storage available", () => {
    expect(loadThemeChoice(null)).toBe("system");
  });

  it("returns a previously saved choice", () => {
    const storage = new FakeStorage();
    saveThemeChoice("dark", storage);
    expect(loadThemeChoice(storage)).toBe("dark");
  });

  it("falls back to system for an unrecognized stored value", () => {
    const storage = new FakeStorage();
    storage.setItem("awic-theme", "purple");
    expect(loadThemeChoice(storage)).toBe("system");
  });

  it("falls back to system when storage throws (private browsing, quota, disabled site data, ...)", () => {
    expect(loadThemeChoice(new ThrowingStorage())).toBe("system");
  });
});

describe("saveThemeChoice", () => {
  it("persists each of the three choices", () => {
    const storage = new FakeStorage();
    for (const choice of ["system", "light", "dark"] as const) {
      saveThemeChoice(choice, storage);
      expect(loadThemeChoice(storage)).toBe(choice);
    }
  });

  it("never throws even when storage throws", () => {
    expect(() => saveThemeChoice("light", new ThrowingStorage())).not.toThrow();
  });
});

describe("resolveTheme", () => {
  it("system follows prefers-color-scheme", () => {
    expect(resolveTheme("system", true)).toBe("dark");
    expect(resolveTheme("system", false)).toBe("light");
  });

  it("light and dark are explicit and ignore the OS preference", () => {
    expect(resolveTheme("light", true)).toBe("light");
    expect(resolveTheme("dark", false)).toBe("dark");
  });
});

describe("applyResolvedTheme", () => {
  function fakeRoot(): { calls: string[]; setAttribute: (n: string, v: string) => void; removeAttribute: (n: string) => void } {
    const calls: string[] = [];
    return {
      calls,
      setAttribute: (n: string, v: string) => calls.push(`set ${n}=${v}`),
      removeAttribute: (n: string) => calls.push(`remove ${n}`),
    };
  }

  it("always sets data-theme to the resolved value — light or dark, never absent", () => {
    const root = fakeRoot();
    applyResolvedTheme("dark", root);
    expect(root.calls).toEqual(["set data-theme=dark"]);
  });

  it("sets data-theme=light for the resolved light theme, including while the choice is system", () => {
    const root = fakeRoot();
    applyResolvedTheme("light", root);
    expect(root.calls).toEqual(["set data-theme=light"]);
  });
});
