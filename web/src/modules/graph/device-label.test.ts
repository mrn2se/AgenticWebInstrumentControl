import { describe, expect, it } from "vitest";
import type { DeviceInfo } from "../../proto/types";
import { collisionNote, deviceDisplayLabel } from "./device-label";

function device(overrides: Partial<DeviceInfo> = {}): DeviceInfo {
  return { id: "3-3.4.2", vid: 0x04b5, pid: 0x602a, label: null, state: "ready", capabilities: [], ...overrides };
}

describe("deviceDisplayLabel", () => {
  it("prefers an explicit label when present", () => {
    expect(deviceDisplayLabel(device({ label: "Bench scope" }))).toBe("Bench scope");
  });

  it("falls back to the bus-path id, never the model name, when there is no label", () => {
    expect(deviceDisplayLabel(device({ id: "3-3.4.2", label: null }))).toBe("3-3.4.2");
  });
});

describe("collisionNote (SPEC §5.1: the 0925:3881 collision)", () => {
  it("flags the colliding VID:PID plainly", () => {
    expect(collisionNote(0x0925, 0x3881)).toMatch(/oscilloscope|logic analyzer/);
  });

  it("says nothing for an unambiguous device", () => {
    expect(collisionNote(0x04b5, 0x602a)).toBeNull();
  });
});
