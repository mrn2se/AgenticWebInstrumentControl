import { describe, expect, it } from "vitest";
import { findSource, groupSources } from "./sources-view";
import type { TerminalSourceInfo } from "../../proto/types";

function source(overrides: Partial<TerminalSourceInfo> & Pick<TerminalSourceInfo, "source" | "kind">): TerminalSourceInfo {
  return { label: overrides.source, available: true, ...overrides };
}

describe("groupSources", () => {
  it("groups serial before log, in fixed order, regardless of input order", () => {
    const sources = [
      source({ source: "log", kind: "log" }),
      source({ source: "serial:1-1:1.0", kind: "serial" }),
    ];
    const groups = groupSources(sources);
    expect(groups.map((g) => g.kind)).toEqual(["serial", "log"]);
  });

  it("omits empty groups", () => {
    const groups = groupSources([source({ source: "log", kind: "log" })]);
    expect(groups).toHaveLength(1);
    expect(groups[0]?.kind).toBe("log");
  });

  it("keeps every source of a kind together", () => {
    const sources = [
      source({ source: "serial:a", kind: "serial" }),
      source({ source: "serial:b", kind: "serial" }),
      source({ source: "log", kind: "log" }),
    ];
    const groups = groupSources(sources);
    expect(groups[0]?.sources).toHaveLength(2);
  });

  it("returns nothing for an empty list", () => {
    expect(groupSources([])).toEqual([]);
  });
});

describe("findSource", () => {
  it("finds by source id", () => {
    const sources = [source({ source: "log", kind: "log" })];
    expect(findSource(sources, "log")?.kind).toBe("log");
  });

  it("returns undefined for an unknown id", () => {
    expect(findSource([], "nope")).toBeUndefined();
  });
});
