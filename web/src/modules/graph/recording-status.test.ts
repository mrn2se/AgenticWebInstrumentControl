import { describe, expect, it } from "vitest";
import type { RecListResult, RecordingSummary } from "../../proto/types";
import { formatRecordingStatus, headSeconds, retainedSeconds } from "./recording-status";

function summary(overrides: Partial<RecordingSummary> = {}): RecordingSummary {
  return {
    rec: 1,
    device: "3-3.4.1.1",
    kind: "digital",
    rate_hz: 1_000_000,
    channels: [],
    first_sample: 0,
    head_sample: 1_000_000,
    bytes: 181_520,
    ratio: 225.9,
    state: "recording",
    ...overrides,
  };
}

function list(recordings: RecordingSummary[], overrides: Partial<RecListResult> = {}): RecListResult {
  return { recordings, budget_mb: 256, used_bytes: 181_520, ...overrides };
}

describe("retainedSeconds", () => {
  it("divides the retained sample span by the rate", () => {
    expect(retainedSeconds(summary({ first_sample: 0, head_sample: 2_000_000, rate_hz: 1_000_000 }))).toBe(2);
  });

  it("accounts for eviction moving first_sample forward", () => {
    expect(retainedSeconds(summary({ first_sample: 500_000, head_sample: 1_500_000, rate_hz: 1_000_000 }))).toBe(1);
  });

  it("is 0 for a zero rate rather than dividing by zero", () => {
    expect(retainedSeconds(summary({ rate_hz: 0 }))).toBe(0);
  });
});

describe("headSeconds", () => {
  it("is the head sample divided by the rate", () => {
    expect(headSeconds(summary({ head_sample: 2_500_000, rate_hz: 1_000_000 }))).toBe(2.5);
  });

  it("is 0 for a zero rate rather than dividing by zero", () => {
    expect(headSeconds(summary({ rate_hz: 0 }))).toBe(0);
  });
});

describe("formatRecordingStatus", () => {
  it("includes span, memory and ratio, but not the plain state (the toolbar badge already shows it)", () => {
    const s = summary();
    const text = formatRecordingStatus(s, list([s]));
    expect(text).not.toContain("recording");
    expect(text).toContain("1.0 s");
    expect(text).toContain("256 MB");
    expect(text).toContain("225.9x");
  });

  it("surfaces the server's own error message rather than a generic label", () => {
    const s = summary({ state: "error", msg: "device lost" });
    const text = formatRecordingStatus(s, list([s]));
    expect(text).toContain("error — device lost");
  });

  it("formats sub-second spans in milliseconds", () => {
    const s = summary({ first_sample: 0, head_sample: 500, rate_hz: 1000 });
    const text = formatRecordingStatus(s, list([s]));
    expect(text).toContain("500 ms");
  });
});
