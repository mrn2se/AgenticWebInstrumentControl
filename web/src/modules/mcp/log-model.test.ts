import { describe, expect, it } from "vitest";
import type { McpLogEntry } from "../../proto/types";
import {
  McpLogModel,
  NO_FILTER,
  entryTitle,
  formatAgo,
  formatBytes,
  formatDuration,
  matchesFilter,
  plural,
} from "./log-model";

function entry(seq: number, overrides: Partial<McpLogEntry> = {}): McpLogEntry {
  return {
    seq,
    t_ms: 1_700_000_000_000 + seq,
    session: "abcd1234",
    client: "Visual Studio Code",
    kind: "request",
    method: "tools/call",
    tool: "survey",
    duration_ms: 12,
    is_error: false,
    summary: '{"device":"3-3"}',
    images: [],
    ...overrides,
  };
}

describe("McpLogModel", () => {
  it("appends in seq order and tracks the last seq", () => {
    const model = new McpLogModel();
    model.merge([entry(1), entry(2)], 2, 2);
    model.merge([entry(3)], 3, 3);
    expect(model.entries.map((e) => e.seq)).toEqual([1, 2, 3]);
    expect(model.lastSeq).toBe(3);
  });

  it("ignores entries it already has", () => {
    const model = new McpLogModel();
    model.merge([entry(1), entry(2)], 2, 2);
    model.merge([entry(2)], 2, 2);
    expect(model.entries).toHaveLength(2);
  });

  it("drops what the server dropped to stay inside its memory bound", () => {
    const model = new McpLogModel();
    model.merge([entry(1), entry(2), entry(3)], 3, 3);
    model.merge([entry(4)], 4, 2);
    expect(model.entries.map((e) => e.seq)).toEqual([3, 4]);
  });

  it("empties when the server's log was cleared, but keeps counting from its seq", () => {
    const model = new McpLogModel();
    model.merge([entry(1), entry(2)], 2, 2);
    expect(model.merge([], 2, 0)).toBe(true);
    expect(model.entries).toHaveLength(0);
    expect(model.lastSeq).toBe(2);
  });

  it("caps the local copy", () => {
    const model = new McpLogModel(2);
    model.merge([entry(1), entry(2), entry(3)], 3, 3);
    expect(model.entries.map((e) => e.seq)).toEqual([2, 3]);
  });
});

describe("matchesFilter", () => {
  it("matches everything with no filter", () => {
    expect(matchesFilter(entry(1), NO_FILTER)).toBe(true);
  });

  it("filters by text across tool, client and summary, ignoring case", () => {
    expect(matchesFilter(entry(1), { ...NO_FILTER, text: "SURVEY" })).toBe(true);
    expect(matchesFilter(entry(1), { ...NO_FILTER, text: "code" })).toBe(true);
    expect(matchesFilter(entry(1), { ...NO_FILTER, text: "3-3" })).toBe(true);
    expect(matchesFilter(entry(1), { ...NO_FILTER, text: "render" })).toBe(false);
  });

  it("filters errors, pictures and sessions", () => {
    expect(matchesFilter(entry(1), { ...NO_FILTER, errorsOnly: true })).toBe(false);
    expect(matchesFilter(entry(1, { is_error: true }), { ...NO_FILTER, errorsOnly: true })).toBe(true);
    expect(matchesFilter(entry(1), { ...NO_FILTER, imagesOnly: true })).toBe(false);
    const withImage = entry(1, { images: [{ mime_type: "image/svg+xml", bytes: 10, kept: true }] });
    expect(matchesFilter(withImage, { ...NO_FILTER, imagesOnly: true })).toBe(true);
    expect(matchesFilter(entry(1), { ...NO_FILTER, session: "other" })).toBe(false);
  });
});

describe("entryTitle", () => {
  it("names tool calls by their tool and lifecycle entries by what happened", () => {
    expect(entryTitle(entry(1))).toBe("survey");
    expect(entryTitle(entry(1, { tool: null, method: "tools/list" }))).toBe("tools/list");
    expect(entryTitle(entry(1, { tool: null, method: "initialize" }))).toBe("initialize");
    expect(entryTitle(entry(1, { kind: "session_end", method: "session" }))).toBe("session end");
    expect(entryTitle(entry(1, { kind: "rejected", tool: null, method: "tools/list" }))).toBe("rejected tools/list");
  });
});

describe("formatting", () => {
  it("formats bytes, durations and ages", () => {
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(2048)).toBe("2.0 KB");
    expect(formatBytes(16 * 1024 * 1024)).toBe("16.0 MB");
    expect(formatDuration(0.2)).toBe("<1 ms");
    expect(formatDuration(41.6)).toBe("42 ms");
    expect(formatDuration(1500)).toBe("1.50 s");
    expect(formatDuration(null)).toBe("");
    expect(formatAgo(0, 5_000)).toBe("5 s ago");
    expect(formatAgo(0, 180_000)).toBe("3 min ago");
  });
});

describe("plural", () => {
  it("counts in words", () => {
    expect(plural(1, "error")).toBe("1 error");
    expect(plural(3, "error")).toBe("3 errors");
    expect(plural(1, "entry")).toBe("1 entry");
    expect(plural(0, "entry")).toBe("0 entries");
  });
});
