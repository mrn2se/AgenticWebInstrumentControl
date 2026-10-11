import { describe, expect, it } from "vitest";
import { serialSourceForDevice } from "./terminal-link";
import type { TerminalSourceInfo } from "../../proto/types";

function source(
  source_: string,
  device: string | undefined,
  kind: TerminalSourceInfo["kind"] = "serial",
): TerminalSourceInfo {
  return { source: source_, kind, label: source_, device, available: true };
}

describe("serialSourceForDevice", () => {
  it("matches a serial source whose device field equals the row's bus-path key", () => {
    const sources = [source("serial:3-3.4.4:1.1", "3-3.4.4"), source("log", undefined, "log")];
    expect(serialSourceForDevice(sources, "3-3.4.4")?.source).toBe("serial:3-3.4.4:1.1");
  });

  it("does not match a different device's bus path", () => {
    const sources = [source("serial:3-3.4.1:1.0", "3-3.4.1")];
    expect(serialSourceForDevice(sources, "3-3.4.4")).toBeUndefined();
  });

  it("does not match a sibling path that merely shares a prefix (3-3.4 vs 3-3.44)", () => {
    const sources = [source("serial:3-3.44:1.0", "3-3.44")];
    expect(serialSourceForDevice(sources, "3-3.4")).toBeUndefined();
  });

  it("ignores non-serial sources even if device happened to be set", () => {
    const sources = [source("log", "log", "log")];
    expect(serialSourceForDevice(sources, "log")).toBeUndefined();
  });

  it("ignores a serial source with no device field", () => {
    const sources = [source("serial:x", undefined)];
    expect(serialSourceForDevice(sources, "x")).toBeUndefined();
  });
});
