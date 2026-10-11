import { describe, expect, it } from "vitest";
import { isValidCustomBaud } from "./serial-settings";

describe("isValidCustomBaud", () => {
  it.each([9600, 115200, 1, 4_000_000])("accepts %d", (value) => {
    expect(isValidCustomBaud(value)).toBe(true);
  });

  it.each([0, -1, 1.5, 4_000_001, NaN])("rejects %d", (value) => {
    expect(isValidCustomBaud(value)).toBe(false);
  });
});
