import { describe, expect, it } from "vitest";
import type { DecodedAnnotation, DecoderInfo } from "../../proto/types";
import {
  decodeRateOf,
  summarizeDecode,
  summaryLine,
  unitsToHexDump,
  unitsToText,
  type DecodedUnit,
} from "./decode-summary";

function byte(value: number, start: number, end = start + 10): DecodedAnnotation {
  return { start_sample: start, end_sample: end, level: 0, kind: { kind: "data", value }, text: `0x${value.toString(16)}` };
}

function framingError(start: number): DecodedAnnotation {
  return { start_sample: start, end_sample: start + 10, level: 0, kind: { kind: "framing_error" }, text: "framing error" };
}

function data(text: string): DecodedUnit[] {
  return [...text].map((c) => ({ kind: "data", value: c.charCodeAt(0) }));
}

const uart: DecoderInfo = {
  id: "uart",
  name: "UART",
  channels: ["rx"],
  options: [{ type: "int", id: "baud", description: "", default: 115200, min: 0, max: null }],
};

describe("summarizeDecode", () => {
  it("counts frames, bytes and errors and spans first start to last end", () => {
    const bit: DecodedAnnotation = { start_sample: 1, end_sample: 2, level: 2, kind: { kind: "start" }, text: "S" };
    const summary = summarizeDecode([byte(0x48, 0), bit, framingError(20), byte(0x69, 40, 50)], 1000);
    expect(summary.frames).toBe(3);
    expect(summary.bytes).toBe(2);
    expect(summary.framingErrors).toBe(1);
    expect(summary.units).toEqual([{ kind: "data", value: 0x48 }, { kind: "error" }, { kind: "data", value: 0x69 }]);
    expect(summary.spanSeconds).toBeCloseTo(0.05);
  });

  it("is empty with no frames", () => {
    const summary = summarizeDecode([], 1000);
    expect(summary.frames).toBe(0);
    expect(summary.spanSeconds).toBeNull();
  });
});

describe("summaryLine", () => {
  it("leads with the rate and says when it was detected", () => {
    const summary = summarizeDecode([byte(0x41, 0), framingError(100)], 1_000_000);
    expect(summaryLine("uart", summary, { value: 9600, detected: true })).toBe(
      "9600 baud (detected) · 1 byte in 110.000 µs · 1 framing error",
    );
  });

  it("counts frames when none carried data", () => {
    const summary = summarizeDecode([framingError(0)], 1000);
    expect(summaryLine("uart", summary, null)).toMatch(/^1 frame in /);
  });
});

describe("decodeRateOf", () => {
  it("prefers the detected rate, then the one sent, then the default", () => {
    expect(decodeRateOf(uart, [{ id: "baud", value: "auto" }], 9600)).toEqual({ value: 9600, detected: true });
    expect(decodeRateOf(uart, [{ id: "baud", value: 57600 }], null)).toEqual({ value: 57600, detected: false });
    expect(decodeRateOf(uart, [], null)).toEqual({ value: 115200, detected: false });
  });

  it("is null for a decoder without a bit rate", () => {
    expect(decodeRateOf({ id: "i2c", name: "I2C", channels: ["scl", "sda"], options: [] }, [], null)).toBeNull();
  });
});

describe("unitsToText", () => {
  it("keeps printable text and folds CR LF into one line break", () => {
    expect(unitsToText(data("ok\r\nnext\rlast\n"))).toBe("ok\nnext\nlast\n");
  });

  it("marks non-printable bytes and broken frames where they were", () => {
    expect(unitsToText([...data("a"), { kind: "data", value: 0x00 }, { kind: "error" }, ...data("b")])).toBe("a·�b");
  });
});

describe("unitsToHexDump", () => {
  it("prints offset, padded values and ASCII per row", () => {
    const dump = unitsToHexDump(data("Hello, world"), 8).split("\n");
    expect(dump).toEqual(["0000  48 65 6c 6c 6f 2c 20 77  Hello, w", "0008  6f 72 6c 64              orld"]);
  });

  it("shows a broken frame as ??", () => {
    // 8 columns of "xx " minus the last space is 23 wide; "?? 41" fills 5 of it.
    expect(unitsToHexDump([{ kind: "error" }, ...data("A")], 8)).toBe(`0000  ?? 41${" ".repeat(18)}  �A`);
  });

  it("widens every column for 9-bit words", () => {
    expect(unitsToHexDump([{ kind: "data", value: 0x1ff }, { kind: "data", value: 0x41 }], 2)).toBe("0000  1ff 041  ·A");
  });
});
