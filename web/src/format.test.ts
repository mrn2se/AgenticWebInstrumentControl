import { describe, expect, it } from "vitest";
import { formatSampleRate, formatSi, formatSiDuration, formatSiFrequency } from "./format";

describe("formatSiDuration (SI units, not '0.0019 s')", () => {
  it("formats a sub-millisecond value in milliseconds, not seconds", () => {
    expect(formatSiDuration(0.001952)).toBe("1.952 ms");
  });

  it("formats a microsecond-scale value in microseconds", () => {
    expect(formatSiDuration(0.0000015)).toBe("1.500 µs");
  });

  it("formats a multi-second value in seconds", () => {
    expect(formatSiDuration(41.0021)).toBe("41.002 s");
  });

  it("formats exactly zero", () => {
    expect(formatSiDuration(0)).toBe("0.000 s");
  });

  it("keeps millisecond resolution at the 's' scale, enough to tell 1 kHz edges apart", () => {
    // Two matches exactly one 1 kHz bit period (1 ms) apart must render as
    // different strings even once the magnitude has crossed into seconds.
    const a = formatSiDuration(3.001);
    const b = formatSiDuration(3.002);
    expect(a).not.toBe(b);
  });

  it("formats a nanosecond-scale value in nanoseconds", () => {
    expect(formatSiDuration(12e-9)).toBe("12.000 ns");
  });
});

describe("formatSiFrequency (avoids exponential notation like '1.000e+6')", () => {
  it("formats a megahertz-scale value in MHz", () => {
    expect(formatSiFrequency(1_000_000)).toBe("1.000 MHz");
  });

  it("formats a kilohertz-scale value in kHz", () => {
    expect(formatSiFrequency(1000)).toBe("1.000 kHz");
  });

  it("formats a sub-hertz value in Hz, not a smaller unit", () => {
    expect(formatSiFrequency(0.5)).toBe("0.500 Hz");
  });

  it("formats zero", () => {
    expect(formatSiFrequency(0)).toBe("0.000 Hz");
  });
});

describe("formatSi", () => {
  it("picks a prefix and keeps the significant digits", () => {
    expect(formatSi(0.001, "s", 4)).toBe("1.000 ms");
    expect(formatSi(8.18705e-7, "s", 3)).toBe("819 ns");
    expect(formatSi(1.64226e-7, "s", 3)).toBe("164 ns");
    expect(formatSi(1000, "Hz", 4)).toBe("1.000 kHz");
    expect(formatSi(2.5, "V", 3)).toBe("2.50 V");
  });

  it("rounds before choosing the prefix", () => {
    expect(formatSi(0.00099996, "s", 4)).toBe("1.000 ms");
  });

  it("keeps the sign", () => {
    expect(formatSi(-0.0123, "V", 3)).toBe("-12.3 mV");
  });

  it("formats zero without a prefix", () => {
    expect(formatSi(0, "V", 3)).toBe("0.00 V");
  });
});

describe("formatSampleRate", () => {
  it("labels rates in samples per second, without trailing zeros", () => {
    expect(formatSampleRate(16_000_000)).toBe("16 MS/s");
    expect(formatSampleRate(20_000)).toBe("20 kS/s");
    expect(formatSampleRate(62_500)).toBe("62.5 kS/s");
    expect(formatSampleRate(500)).toBe("500 S/s");
  });
});
