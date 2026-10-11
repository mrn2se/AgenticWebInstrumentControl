import { describe, expect, it } from "vitest";
import type { RecordingSummary } from "../../proto/types";
import { pickRecordingToAttach, startedItsOwnRecording } from "./live-attach";

function summary(overrides: Partial<RecordingSummary> = {}): RecordingSummary {
  return {
    rec: 1,
    device: "3-3.4.1.1",
    kind: "digital",
    rate_hz: 1_000_000,
    channels: [],
    first_sample: 0,
    head_sample: 1000,
    bytes: 100,
    ratio: 2,
    state: "recording",
    ...overrides,
  };
}

describe("pickRecordingToAttach", () => {
  it("finds a recording in progress for the device", () => {
    const rec = summary();
    expect(pickRecordingToAttach([rec], "3-3.4.1.1")).toBe(rec);
  });

  it("ignores recordings for other devices", () => {
    const rec = summary({ device: "3-3.4.2" });
    expect(pickRecordingToAttach([rec], "3-3.4.1.1")).toBeUndefined();
  });

  it("ignores a stopped recording for the same device — a fresh rec.start is wanted instead", () => {
    const rec = summary({ state: "stopped" });
    expect(pickRecordingToAttach([rec], "3-3.4.1.1")).toBeUndefined();
  });

  it("ignores an errored recording for the same device", () => {
    const rec = summary({ state: "error", msg: "device lost" });
    expect(pickRecordingToAttach([rec], "3-3.4.1.1")).toBeUndefined();
  });

  it("returns undefined when there are no recordings at all", () => {
    expect(pickRecordingToAttach([], "3-3.4.1.1")).toBeUndefined();
  });
});

// B-68: the tab deletes the recording it started and leaves alone one it merely
// attached to. The call site asked `existing === null`, and `pickRecordingToAttach`
// is an `Array.find`, so it answers `undefined` - false either way, so no
// recording was ever treated as the tab's own and nothing was ever cleaned up.
// Found by tracing the client's own WebSocket traffic during the start/stop run:
// `rec.stop` went out and `rec.delete` never did.
describe("startedItsOwnRecording", () => {
  it("is true only when nothing was attached to", () => {
    expect(startedItsOwnRecording(undefined)).toBe(true);
    expect(startedItsOwnRecording(summary({ rec: 7 }))).toBe(false);
  });

  it("answers what pickRecordingToAttach actually returns", () => {
    // The pairing that was broken, through the same call the module makes.
    expect(startedItsOwnRecording(pickRecordingToAttach([], "3-3.4.1.1"))).toBe(true);
    expect(startedItsOwnRecording(pickRecordingToAttach([summary()], "3-3.4.1.1"))).toBe(false);
  });
});
