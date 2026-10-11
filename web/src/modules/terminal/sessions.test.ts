import { describe, expect, it } from "vitest";
import {
  applySessionState,
  isWritable,
  removeSession,
  stateBadge,
  upsertSession,
  type SessionViewModel,
} from "./sessions";

function session(overrides: Partial<SessionViewModel> = {}): SessionViewModel {
  return {
    sessionId: 1,
    source: "serial:1-1:1.0",
    kind: "serial",
    label: "ttyACM0",
    state: "open",
    ...overrides,
  };
}

describe("isWritable", () => {
  it("is writable for an open serial session", () => {
    expect(isWritable(session())).toBe(true);
  });

  it("is never writable for a log session", () => {
    expect(isWritable(session({ kind: "log" }))).toBe(false);
  });

  it("is not writable while waiting or closed", () => {
    expect(isWritable(session({ state: "waiting" }))).toBe(false);
    expect(isWritable(session({ state: "closed" }))).toBe(false);
  });
});

describe("stateBadge", () => {
  it("labels each state", () => {
    expect(stateBadge("open")).toBe("open");
    expect(stateBadge("waiting")).toBe("waiting…");
    expect(stateBadge("closed")).toBe("closed");
  });
});

describe("session map operations", () => {
  it("upsertSession adds and updates without mutating the input map", () => {
    const empty = new Map();
    const withOne = upsertSession(empty, session());
    expect(empty.size).toBe(0);
    expect(withOne.size).toBe(1);

    const updated = upsertSession(withOne, session({ state: "waiting" }));
    expect(withOne.get(1)?.state).toBe("open");
    expect(updated.get(1)?.state).toBe("waiting");
  });

  it("removeSession drops the entry and is a no-op for an unknown id", () => {
    const withOne = upsertSession(new Map(), session());
    const removed = removeSession(withOne, 1);
    expect(removed.size).toBe(0);
    expect(removeSession(removed, 999)).toBe(removed);
  });

  it("applySessionState updates only a session this connection has open", () => {
    const withOne = upsertSession(new Map(), session());
    const updated = applySessionState(withOne, 1, "waiting");
    expect(updated.get(1)?.state).toBe("waiting");

    const unchanged = applySessionState(withOne, 999, "closed");
    expect(unchanged).toBe(withOne);
  });
});
