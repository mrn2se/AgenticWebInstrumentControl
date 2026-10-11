import { describe, expect, it } from "vitest";
import { RpcError } from "../../ws/errors";
import {
  formatMatchTimestamp,
  formatWaitOutcome,
  searchOutcomeFromError,
  searchOutcomeFromResult,
  searchResultSummary,
  waitOutcomeFromError,
  waitOutcomeFromResult,
} from "./search-ui";

describe("searchOutcomeFromResult / searchResultSummary", () => {
  it("summarizes zero matches", () => {
    const outcome = searchOutcomeFromResult({ matches: [], truncated: false, searched: { t0_s: 0, t1_s: 1 }, glitches: 0 });
    expect(outcome.status).toBe("matches");
    if (outcome.status !== "matches") throw new Error("unreachable");
    expect(searchResultSummary(outcome)).toBe("no matches");
  });

  it("summarizes one match with singular wording", () => {
    const outcome = searchOutcomeFromResult({
      matches: [{ t_s: 3.214, sample: 3214 }],
      truncated: false,
      searched: { t0_s: 0, t1_s: 1 },
      glitches: 0,
    });
    if (outcome.status !== "matches") throw new Error("unreachable");
    expect(searchResultSummary(outcome)).toBe("1 match");
  });

  it("flags truncation", () => {
    const outcome = searchOutcomeFromResult({
      matches: [{ t_s: 0, sample: 0 }, { t_s: 1, sample: 1 }],
      truncated: true,
      searched: { t0_s: 0, t1_s: 1 },
      glitches: 0,
    });
    if (outcome.status !== "matches") throw new Error("unreachable");
    expect(searchResultSummary(outcome)).toContain("truncated");
  });
});

describe("formatMatchTimestamp", () => {
  it("formats with an SI unit picked from magnitude, not always seconds", () => {
    expect(formatMatchTimestamp({ t_s: 3.2140123, sample: 1 })).toBe("3.214 s");
  });

  it("formats a sub-millisecond match in milliseconds, e.g. '1.952 ms' not '0.0019 s'", () => {
    expect(formatMatchTimestamp({ t_s: 0.001952, sample: 1 })).toBe("1.952 ms");
  });
});

describe("searchOutcomeFromError", () => {
  it("surfaces the server's own error message verbatim (SPEC §2.9.1)", () => {
    const outcome = searchOutcomeFromError(new RpcError({ code: "invalid_pattern", msg: "unexpected token 'x' at column 4" }));
    expect(outcome).toEqual({ status: "error", message: "unexpected token 'x' at column 4" });
  });

  it("falls back to a generic Error's message", () => {
    expect(searchOutcomeFromError(new Error("boom"))).toEqual({ status: "error", message: "boom" });
  });
});

describe("wait outcomes", () => {
  it("matched", () => {
    const outcome = waitOutcomeFromResult({ match: { t_s: 41.0021, sample: 41002100 } });
    expect(outcome).toEqual({ status: "matched", match: { t_s: 41.0021, sample: 41002100 } });
    expect(formatWaitOutcome(outcome)).toBe("matched at 41.002 s");
  });

  it("timed out", () => {
    const outcome = waitOutcomeFromResult({ timeout: true });
    expect(outcome).toEqual({ status: "timed-out" });
    expect(formatWaitOutcome(outcome)).toBe("timed out");
  });

  it("error path uses the server's message", () => {
    const outcome = waitOutcomeFromError(new RpcError({ code: "rec_not_found", msg: "no recording 9" }));
    expect(formatWaitOutcome(outcome)).toBe("no recording 9");
  });
});
