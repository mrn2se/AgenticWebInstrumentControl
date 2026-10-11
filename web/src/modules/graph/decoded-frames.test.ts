import { describe, expect, it } from "vitest";
import { annotationLaneFor, decodedFrameRows, formatDecodedFrameTimestamp } from "./decoded-frames";
import type { DecodedAnnotation } from "../../proto/types";

function ann(over: Partial<DecodedAnnotation>): DecodedAnnotation {
  return {
    start_sample: 0,
    end_sample: 10,
    level: 0,
    kind: { kind: "data", value: 0x41 },
    text: "0x41",
    ...over,
  };
}

describe("decodedFrameRows", () => {
  it("keeps only level-0 (frame) annotations", () => {
    const anns = [
      ann({ level: 0, text: "S" }),
      ann({ level: 1, text: "0x50 W" }),
      ann({ level: 2, text: "A" }),
      ann({ level: 0, text: "P" }),
    ];
    const rows = decodedFrameRows(anns, 1_000_000);
    expect(rows.map((r) => r.label)).toEqual(["S", "P"]);
  });

  it("converts sample indices to seconds using the given rate", () => {
    const anns = [ann({ start_sample: 500_000, end_sample: 600_000 })];
    const rows = decodedFrameRows(anns, 1_000_000);
    expect(rows[0]!.tSeconds).toBeCloseTo(0.5, 9);
  });

  it("indexes rows in order, starting at 0", () => {
    const anns = [ann({}), ann({})];
    const rows = decodedFrameRows(anns, 1_000_000);
    expect(rows.map((r) => r.index)).toEqual([0, 1]);
  });

  it("never divides by zero for a degenerate rate", () => {
    const anns = [ann({ start_sample: 42 })];
    expect(() => decodedFrameRows(anns, 0)).not.toThrow();
  });
});

describe("formatDecodedFrameTimestamp", () => {
  it("formats using the shared SI-duration formatter", () => {
    const row = { index: 0, tSeconds: 0.5, label: "x", startSample: 0, endSample: 1 };
    expect(formatDecodedFrameTimestamp(row)).toMatch(/500\.000 ms|0\.500 s/);
  });
});

describe("annotationLaneFor", () => {
  it("carries the given channel and converts every annotation to a time-domain box", () => {
    const anns = [ann({ start_sample: 100, end_sample: 200, level: 1, text: "0x41" })];
    const lane = annotationLaneFor(5, anns, 1_000_000);
    expect(lane.channel).toBe(5);
    expect(lane.boxes).toEqual([{ t0: 0.0001, t1: 0.0002, level: 1, label: "0x41" }]);
  });

  it("produces an empty lane for no annotations", () => {
    expect(annotationLaneFor(3, [], 1_000_000).boxes).toEqual([]);
  });
});

// B-71: a triggered scope capture showed `256.000 µs — 0x100 [8] …` in the
// frame list while the plot drew that frame at ~6.6 ms, because a decoder
// counts samples from the start of what it was given and the plot places the
// capture at the stream time its frames carry. One axis now.
describe("the decoded span's origin on the plot's axis", () => {
  const annotations: DecodedAnnotation[] = [
    ann({ start_sample: 2048, end_sample: 4096, level: 0, text: "0x100 [8]" }),
    ann({ start_sample: 2048, end_sample: 2100, level: 1, text: "id" }),
  ];

  it("places frame rows where the plot draws them", () => {
    const captureStartsAt = 6.6e-3; // the capture's own t0 within the stream
    const [row] = decodedFrameRows(annotations, 8_000_000, captureStartsAt);
    expect(row).toBeDefined();
    // 2048 samples at 8 MS/s is 256 µs *into the capture*, and the capture
    // starts 6.6 ms into the stream.
    expect(row!.tSeconds).toBeCloseTo(6.6e-3 + 256e-6, 12);
    expect(formatDecodedFrameTimestamp(row!)).not.toBe("256.000 µs");
  });

  it("shifts the annotation boxes by the same origin", () => {
    const lane = annotationLaneFor(4, annotations, 8_000_000, 6.6e-3);
    expect(lane.boxes[0]!.t0).toBeCloseTo(6.6e-3 + 256e-6, 12);
    expect(lane.boxes[0]!.t1).toBeCloseTo(6.6e-3 + 512e-6, 12);
    // Rows and boxes have to agree, or clicking a row would centre the plot
    // somewhere the frame is not.
    const [row] = decodedFrameRows(annotations, 8_000_000, 6.6e-3);
    expect(lane.boxes[0]!.t0).toBeCloseTo(row!.tSeconds, 12);
  });

  it("defaults to no shift, which is what a recording's own timeline needs", () => {
    const [row] = decodedFrameRows(annotations, 8_000_000);
    expect(row!.tSeconds).toBeCloseTo(256e-6, 12);
    expect(annotationLaneFor(4, annotations, 8_000_000).boxes[0]!.t0).toBeCloseTo(256e-6, 12);
  });
});
