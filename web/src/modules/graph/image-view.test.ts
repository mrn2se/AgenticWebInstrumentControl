import { describe, expect, it } from "vitest";

import { imageUnavailableReason, imageViewUrl, isSingleChannel } from "./image-view";

describe("imageViewUrl", () => {
  it("sends only what is set, leaving the server's defaults alone", () => {
    const url = imageViewUrl({ capture: "capture://abc", kind: "waveform" });
    expect(url).toBe("/api/render?capture=capture%3A%2F%2Fabc&kind=waveform");
  });

  it("joins channels the way the endpoint parses them", () => {
    const url = imageViewUrl({ capture: "c", kind: "eye", channels: [0, 3] });
    expect(new URLSearchParams(url.split("?")[1]).get("channels")).toBe("0,3");
  });

  it("carries the window and the size when they are given", () => {
    const query = new URLSearchParams(
      imageViewUrl({ capture: "c", kind: "waveform", t0_s: 0.01, t1_s: 0.02, width: 1600, height: 900 }).split("?")[1],
    );
    expect(query.get("t0_s")).toBe("0.01");
    expect(query.get("t1_s")).toBe("0.02");
    expect(query.get("width")).toBe("1600");
    expect(query.get("height")).toBe("900");
  });

  it("drops a non-finite number rather than sending NaN", () => {
    const query = new URLSearchParams(
      imageViewUrl({ capture: "c", kind: "waveform", t0_s: Number.NaN }).split("?")[1],
    );
    expect(query.has("t0_s")).toBe(false);
  });
});

describe("imageUnavailableReason", () => {
  it("says a capture is needed before anything can be drawn", () => {
    expect(imageUnavailableReason(null, "waveform", [0])).toMatch(/Run a capture/);
  });

  it("allows a waveform of any number of channels", () => {
    expect(imageUnavailableReason("c", "waveform", [])).toBeNull();
    expect(imageUnavailableReason("c", "waveform", [0, 1, 2])).toBeNull();
  });

  it("explains the one-channel rule with the count that is wrong, per view", () => {
    for (const kind of ["eye", "histogram", "anomalies"] as const) {
      expect(imageUnavailableReason("c", kind, [0])).toBeNull();
      expect(imageUnavailableReason("c", kind, [])).toMatch(/none is selected/);
      expect(imageUnavailableReason("c", kind, [0, 1])).toMatch(/2 are selected/);
      expect(imageUnavailableReason("c", kind, [0, 1])).toContain(kind);
    }
  });

  it("knows which views need a channel chosen", () => {
    expect(isSingleChannel("waveform")).toBe(false);
    for (const kind of ["eye", "histogram", "anomalies"] as const) {
      expect(isSingleChannel(kind)).toBe(true);
    }
  });
});
