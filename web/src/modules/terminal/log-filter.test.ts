import { describe, expect, it } from "vitest";
import { filterLines, lineLevel, lineMatchesFilter } from "./log-filter";

describe("lineLevel", () => {
  it("reads the padded [LEVEL] prefix used by the server's log lines", () => {
    expect(lineLevel("[ INFO] awic: hello")).toBe("info");
    expect(lineLevel("[ WARN] something")).toBe("warn");
    expect(lineLevel("[ERROR] oops")).toBe("error");
    expect(lineLevel("[DEBUG] detail")).toBe("debug");
    expect(lineLevel("[TRACE] very detailed")).toBe("trace");
  });

  it("returns null for a line with no recognizable tag", () => {
    expect(lineLevel("[session waiting: device disconnected]")).toBeNull();
    expect(lineLevel("plain text")).toBeNull();
  });
});

describe("lineMatchesFilter", () => {
  it("'all' keeps everything", () => {
    expect(lineMatchesFilter("[ INFO] x", "all")).toBe(true);
    expect(lineMatchesFilter("no tag", "all")).toBe(true);
  });

  it("keeps lines at or above the selected floor, drops below it", () => {
    expect(lineMatchesFilter("[ WARN] x", "warn")).toBe(true);
    expect(lineMatchesFilter("[ERROR] x", "warn")).toBe(true);
    expect(lineMatchesFilter("[ INFO] x", "warn")).toBe(false);
  });

  it("never hides a line it can't classify", () => {
    expect(lineMatchesFilter("[session open]", "error")).toBe(true);
  });
});

describe("filterLines", () => {
  it("filters a buffer down to the matching lines, preserving order", () => {
    const lines = ["[ INFO] a", "[ WARN] b", "[ERROR] c", "[ INFO] d"];
    expect(filterLines(lines, "warn")).toEqual(["[ WARN] b", "[ERROR] c"]);
    expect(filterLines(lines, "error")).toEqual(["[ERROR] c"]);
    expect(filterLines(lines, "all")).toEqual(lines);
  });
});
