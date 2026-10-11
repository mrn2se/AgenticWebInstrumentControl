import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { WsClient } from "./client";
import type { SocketLike } from "./socket";

class MockSocket implements SocketLike {
  onopen: (() => void) | null = null;
  onclose: ((code: number, reason: string) => void) | null = null;
  onerror: (() => void) | null = null;
  onmessage: ((data: string | ArrayBuffer) => void) | null = null;
  readonly sent: Array<string | ArrayBuffer | ArrayBufferView<ArrayBuffer>> = [];
  closed = false;

  send(data: string | ArrayBuffer | ArrayBufferView<ArrayBuffer>): void {
    this.sent.push(data);
  }

  close(): void {
    this.closed = true;
  }

  open(): void {
    this.onopen?.();
  }

  receive(data: string | ArrayBuffer): void {
    this.onmessage?.(data);
  }

  simulateClose(code = 1006, reason = "connection lost"): void {
    this.onclose?.(code, reason);
  }
}

function sentRequests(socket: MockSocket): Array<Record<string, unknown>> {
  return socket.sent.map((raw) => {
    if (typeof raw !== "string") throw new Error("expected a text control frame");
    return JSON.parse(raw) as Record<string, unknown>;
  });
}

describe("WsClient request/response correlation", () => {
  let sockets: MockSocket[];
  let client: WsClient;

  beforeEach(() => {
    sockets = [];
    client = new WsClient({
      url: "ws://test/ws",
      socketFactory: () => {
        const socket = new MockSocket();
        sockets.push(socket);
        return socket;
      },
    });
  });

  it("resolves the matching promise when a response with the same id arrives", async () => {
    client.connect();
    sockets[0]?.open();

    const promise = client.send("device.list", {});
    const [req] = sentRequests(sockets[0]!);
    expect(req?.op).toBe("device.list");
    expect(typeof req?.id).toBe("number");

    sockets[0]?.receive(JSON.stringify({ v: 1, id: req?.id, ok: true, result: { devices: [] } }));

    await expect(promise).resolves.toEqual({ devices: [] });
  });

  it("assigns incrementing ids to concurrent requests and resolves each independently", async () => {
    client.connect();
    sockets[0]?.open();

    const p1 = client.send("device.list", {});
    const p2 = client.send("usb.tree", {});
    const [req1, req2] = sentRequests(sockets[0]!);
    expect(req1?.id).not.toBe(req2?.id);

    // Respond out of order to prove correlation is by id, not send order.
    sockets[0]?.receive(JSON.stringify({ v: 1, id: req2?.id, ok: true, result: { roots: [] } }));
    sockets[0]?.receive(JSON.stringify({ v: 1, id: req1?.id, ok: true, result: { devices: [] } }));

    await expect(p1).resolves.toEqual({ devices: [] });
    await expect(p2).resolves.toEqual({ roots: [] });
  });

  it("sends usb.protect requests and resolves with the UsbProtectResult (SPEC §4.4)", async () => {
    client.connect();
    sockets[0]?.open();

    const promise = client.send("usb.protect", { hub: "3-3", port: 4, protected: false });
    const [req] = sentRequests(sockets[0]!);
    expect(req).toMatchObject({ op: "usb.protect", hub: "3-3", port: 4, protected: false });

    sockets[0]?.receive(
      JSON.stringify({
        v: 1,
        id: req?.id,
        ok: true,
        result: { hub: "3-3", port: 4, protected: false, user_choice: false },
      }),
    );

    await expect(promise).resolves.toEqual({ hub: "3-3", port: 4, protected: false, user_choice: false });
  });

  it("rejects with the server error when a response has ok: false", async () => {
    client.connect();
    sockets[0]?.open();

    const promise = client.send("usb.power", { hub: "3-3", port: 4, on: false });
    const [req] = sentRequests(sockets[0]!);

    sockets[0]?.receive(
      JSON.stringify({
        v: 1,
        id: req?.id,
        ok: false,
        error: { code: "port_protected", msg: "port 4 feeds the instrument hub" },
      }),
    );

    await expect(promise).rejects.toThrow(/port_protected/);
  });

  it("surfaces ok:false responses centrally, not just to the original caller", async () => {
    client.connect();
    sockets[0]?.open();

    const notices: Array<{ code: string }> = [];
    client.errors.on((notice) => notices.push(notice));

    const promise = client.send("usb.power", { hub: "3-3", port: 4, on: false });
    const [req] = sentRequests(sockets[0]!);
    sockets[0]?.receive(
      JSON.stringify({ v: 1, id: req?.id, ok: false, error: { code: "port_protected", msg: "no" } }),
    );

    await expect(promise).rejects.toThrow();
    expect(notices).toEqual([{ id: req?.id, code: "port_protected", msg: "no" }]);
  });

  it("dispatches messages with no id to event subscribers, not to any pending request", () => {
    client.connect();
    sockets[0]?.open();

    const received: unknown[] = [];
    client.events.on((event) => received.push(event));

    sockets[0]?.receive(
      JSON.stringify({ v: 1, event: "device.added", device: { id: "3-3.4.2", vid: 1, pid: 2 } }),
    );

    expect(received).toHaveLength(1);
    expect(received[0]).toMatchObject({ event: "device.added" });
  });

  it("rejects send() when there is no open socket", async () => {
    await expect(client.send("device.list", {})).rejects.toThrow(/not connected/);
  });
});

describe("WsClient request timeout", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("rejects a request that never receives a response, instead of hanging", async () => {
    const sockets: MockSocket[] = [];
    const client = new WsClient({
      url: "ws://test/ws",
      requestTimeoutMs: 1000,
      socketFactory: () => {
        const socket = new MockSocket();
        sockets.push(socket);
        return socket;
      },
    });
    client.connect();
    sockets[0]?.open();

    const promise = client.send("device.list", {});
    const assertion = expect(promise).rejects.toThrow(/timed out/);
    await vi.advanceTimersByTimeAsync(1000);
    await assertion;
  });

  it("fails all pending requests when the connection drops before a response arrives", async () => {
    const sockets: MockSocket[] = [];
    const client = new WsClient({
      url: "ws://test/ws",
      socketFactory: () => {
        const socket = new MockSocket();
        sockets.push(socket);
        return socket;
      },
    });
    client.connect();
    sockets[0]?.open();

    const promise = client.send("device.list", {});
    const assertion = expect(promise).rejects.toThrow();
    sockets[0]?.simulateClose();
    await assertion;
  });
});

describe("WsClient reconnect backoff", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("reconnects after an unexpected close, waiting a growing backoff between attempts", () => {
    const sockets: MockSocket[] = [];
    const client = new WsClient({
      url: "ws://test/ws",
      backoff: { initialMs: 100, maxMs: 1000, factor: 2, jitterRatio: 0 },
      random: () => 0.5,
      socketFactory: () => {
        const socket = new MockSocket();
        sockets.push(socket);
        return socket;
      },
    });
    const states: string[] = [];
    client.connectionState.on((state) => states.push(state));

    client.connect();
    expect(sockets).toHaveLength(1);

    sockets[0]?.simulateClose();
    expect(states).toContain("reconnecting");
    expect(sockets).toHaveLength(1); // no new socket until the backoff elapses

    vi.advanceTimersByTime(99);
    expect(sockets).toHaveLength(1);
    vi.advanceTimersByTime(1);
    expect(sockets).toHaveLength(2);

    sockets[1]?.simulateClose();
    vi.advanceTimersByTime(199); // second attempt backs off to 200ms
    expect(sockets).toHaveLength(2);
    vi.advanceTimersByTime(1);
    expect(sockets).toHaveLength(3);
  });

  it("resets the backoff attempt counter after a successful connection", () => {
    const sockets: MockSocket[] = [];
    const client = new WsClient({
      url: "ws://test/ws",
      backoff: { initialMs: 100, maxMs: 1000, factor: 2, jitterRatio: 0 },
      random: () => 0.5,
      socketFactory: () => {
        const socket = new MockSocket();
        sockets.push(socket);
        return socket;
      },
    });

    client.connect();
    sockets[0]?.simulateClose(); // attempt 0 scheduled at 100ms
    vi.advanceTimersByTime(100);
    expect(sockets).toHaveLength(2);

    sockets[1]?.open(); // success resets the attempt counter
    sockets[1]?.simulateClose();
    vi.advanceTimersByTime(99);
    expect(sockets).toHaveLength(2);
    vi.advanceTimersByTime(1); // back to the initial 100ms delay, not 200ms
    expect(sockets).toHaveLength(3);
  });

  it("does not reconnect after an explicit close()", () => {
    const sockets: MockSocket[] = [];
    const client = new WsClient({
      url: "ws://test/ws",
      socketFactory: () => {
        const socket = new MockSocket();
        sockets.push(socket);
        return socket;
      },
    });

    client.connect();
    client.close();
    vi.advanceTimersByTime(60_000);
    expect(sockets).toHaveLength(1);
    expect(client.getConnectionState()).toBe("offline");
  });
});
