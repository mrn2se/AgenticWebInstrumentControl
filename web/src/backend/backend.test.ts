import { describe, expect, it } from "vitest";
import { demoBackend, selfBackend } from "./backend";

describe("backends", () => {
  it("keeps the page's own server relative", () => {
    const backend = selfBackend({ protocol: "https:", host: "bench.lan:8080" });
    expect(backend.wsUrl).toBe("wss://bench.lan:8080/ws");
    expect(backend.resourceUrl("/api/render")).toBe("/api/render");
  });

  it("gives the demo no HTTP side", () => {
    const backend = demoBackend(() => {
      throw new Error("not opened here");
    });
    expect(backend.resourceUrl("/api/render")).toBeNull();
  });
});
