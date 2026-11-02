import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { DecodedAnnotation } from "../proto/types";
import { WsClient } from "../ws/client";
import type { DecodedFrame } from "../ws/frame";
import type { DecodedViewFrame } from "../ws/view-frame";
import { demoSocketFactory } from ".";
import { buildAnalogView } from "./server";
import { LogicScene, PERIOD_S, WIRING } from "./scene";
import { LOGIC_ID, SCOPE_ID } from "./usb-bench";

/** The tests talk to the demo through the real client, so its frames go through the real decoders. */
function connect(): WsClient {
  const client = new WsClient({ url: "demo://x", socketFactory: demoSocketFactory() });
  client.connect();
  return client;
}

async function flush(ms = 200): Promise<void> {
  await vi.advanceTimersByTimeAsync(ms);
}

async function call<T>(promise: Promise<T>, ms = 100): Promise<T> {
  // A refusal arrives during the flush, before the caller's own handler is attached.
  promise.catch(() => {});
  await flush(ms);
  return promise;
}

/** Reads a UART line back off the packed samples by itself: proof the wire holds what the annotations claim. */
function uartBytesFromSamples(samples: Uint8Array, rate: number, bit: number): number[] {
  const perBit = rate / 115200;
  const out: number[] = [];
  let i = 1;
  const level = (idx: number): number => (samples[Math.floor(idx)]! >> bit) & 1;
  while (i < samples.length) {
    if (level(i - 1) === 1 && level(i) === 0) {
      let value = 0;
      for (let b = 0; b < 8; b++) value |= level(i + perBit * (1.5 + b)) << b;
      out.push(value);
      i += Math.floor(perBit * 9.5);
    } else {
      i += 1;
    }
  }
  return out;
}

describe("the simulated server", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-11-02T12:00:00Z"));
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("connects and lists both instruments", async () => {
    const client = connect();
    await flush();
    expect(client.getConnectionState()).toBe("connected");
    const { devices } = await call(client.send("device.list", {}));
    expect(devices.map((d) => d.id).sort()).toEqual([LOGIC_ID, SCOPE_ID].sort());
    client.close();
  });

  async function captureLogic(client: WsClient, rate: number, samples: number) {
    const frames: DecodedFrame[] = [];
    client.frames.on((f) => frames.push(f));
    const configured = await call(client.send("acq.configure", { device: LOGIC_ID, cfg: { rate_hz: rate, channels: [0, 1, 2, 3, 4, 5, 6, 7], samples } }));
    const started = await call(client.send("acq.start", { device: LOGIC_ID }));
    await flush(500);
    return { configured, started, frames };
  }

  it("streams a capture whose wires hold what its decode reports", async () => {
    const client = connect();
    await flush();
    const { started, frames } = await captureLogic(client, 8_000_000, 160_000);
    const total = frames.reduce((n, f) => n + f.header.sampleCount, 0);
    expect(total).toBe(160_000);
    expect(frames.every((f) => f.header.encoding === "packed-digital")).toBe(true);

    const samples = new Uint8Array(160_000);
    let at = 0;
    for (const f of frames) {
      samples.set(new Uint8Array(f.payload.buffer, f.payload.byteOffset, f.header.sampleCount), at);
      at += f.header.sampleCount;
    }
    const fromWire = String.fromCharCode(...uartBytesFromSamples(samples, 8_000_000, 0));
    expect(fromWire).toBe("temp=23.4C rpm=1480 ok\r\n");

    const decoded = await call(client.send("capture.decode", { capture: started.capture, decoder: "uart", channels: [{ role: "rx", channel: 0 }], options: [] }));
    const text = String.fromCharCode(...(decoded.annotations as DecodedAnnotation[]).map((a) => (a.kind.kind === "data" ? a.kind.value : 0)));
    expect(text).toBe(fromWire);
    client.close();
  });

  it("decodes I2C and SPI on their wires, and finds nothing elsewhere", async () => {
    const client = connect();
    await flush();
    const { started } = await captureLogic(client, 8_000_000, 160_000);
    const i2c = await call(client.send("capture.decode", { capture: started.capture, decoder: "i2c", channels: [{ role: "scl", channel: 1 }, { role: "sda", channel: 2 }], options: [] }));
    expect((i2c.annotations ?? []).map((a) => a.text).slice(0, 4)).toEqual(["S", "0x48 W", "A", "0x00"]);
    const spi = await call(client.send("capture.decode", { capture: started.capture, decoder: "spi", channels: [{ role: "clk", channel: 4 }, { role: "mosi", channel: 5 }, { role: "miso", channel: 6 }, { role: "cs", channel: 3 }], options: [] }));
    expect((spi.annotations ?? []).some((a) => a.text === "MOSI=0x9F MISO=0xFF")).toBe(true);
    const wrong = await call(client.send("capture.decode", { capture: started.capture, decoder: "uart", channels: [{ role: "rx", channel: 7 }], options: [] }));
    expect(wrong.annotations).toEqual([]);
    expect(wrong.note).toContain("D0");
    client.close();
  });

  it("measures the clock line as what it is", async () => {
    const client = connect();
    await flush();
    const { started } = await captureLogic(client, 1_000_000, 200_000);
    const { results } = await call(client.send("capture.measure", { capture: started.capture, channel: WIRING.clock, measurements: ["frequency", "duty", "vpp"] }));
    const value = (kind: string): number => {
      const r = results.find((x) => x.kind === kind);
      return r && "value" in r ? r.value : Number.NaN;
    };
    expect(value("frequency")).toBeCloseTo(1000, 3);
    expect(value("duty")).toBeCloseTo(25, 1);
    expect(results.find((x) => x.kind === "vpp")).toHaveProperty("unavailable");
    client.close();
  });

  it("captures the scope in calibrated volts", async () => {
    const client = connect();
    await flush();
    const configured = await call(client.send("acq.configure", { device: SCOPE_ID, cfg: { rate_hz: 1_000_000, channels: [0, 1], samples: 20_000, ranges_v: [2.5, 2.5] } }));
    expect(configured.calibration[0]?.unit).toBe("V");
    const started = await call(client.send("acq.start", { device: SCOPE_ID }));
    await flush(300);
    const { results } = await call(client.send("capture.measure", { capture: started.capture, channel: 0, measurements: ["vpp", "frequency"] }));
    const vpp = results.find((r) => r.kind === "vpp");
    const freq = results.find((r) => r.kind === "frequency");
    expect(vpp && "value" in vpp ? vpp.value : 0).toBeGreaterThan(1.9);
    expect(freq && "value" in freq ? freq.value : 0).toBeCloseTo(1000, 0);
    client.close();
  });

  it("places a triggered capture's edge at its midpoint", async () => {
    const client = connect();
    await flush();
    const frames: DecodedFrame[] = [];
    client.frames.on((f) => frames.push(f));
    await call(client.send("acq.configure", { device: LOGIC_ID, cfg: { rate_hz: 1_000_000, channels: [7], samples: 2000, trigger: { kind: "edge", channel: 7, slope: "rising" } } }));
    await call(client.send("acq.start", { device: LOGIC_ID }));
    await flush(300);
    const samples = new Uint8Array(frames[0]!.payload.buffer, frames[0]!.payload.byteOffset, 2000);
    expect((samples[999]! >> 7) & 1).toBe(0);
    expect((samples[1000]! >> 7) & 1).toBe(1);
    client.close();
  });

  it("holds a device for one user at a time", async () => {
    const client = connect();
    await flush();
    const rec = await call(client.send("rec.start", { device: LOGIC_ID, cfg: { rate_hz: 1_000_000, channels: [0], samples: 0 } }));
    await call(client.send("acq.configure", { device: LOGIC_ID, cfg: { rate_hz: 1_000_000, channels: [0], samples: 100 } }));
    await expect(call(client.send("acq.start", { device: LOGIC_ID }))).rejects.toThrow(/busy/);
    await call(client.send("rec.stop", { rec: rec.rec }));
    client.close();
  });

  it("records, follows, searches and snapshots", async () => {
    const client = connect();
    await flush();
    const views: DecodedViewFrame[] = [];
    client.viewFrames.on((f) => views.push(f));
    const rec = await call(client.send("rec.start", { device: LOGIC_ID, cfg: { rate_hz: 1_000_000, channels: [0, 7], samples: 0 } }));
    await flush(1000);
    const view = await call(client.send("rec.view", { rec: rec.rec, follow_s: 0.02, columns: 1000, fps: 30 }));
    await flush(400);
    expect(views.length).toBeGreaterThan(3);
    expect(views.at(-1)?.header.viewId).toBe(view.view);
    expect(views.at(-1)?.header.flags.follow).toBe(true);

    const found = await call(client.send("rec.search", { rec: rec.rec, pattern: "D7 rising", max_matches: 2 }));
    expect(found.matches).toHaveLength(2);
    expect(found.matches[1]!.t_s - found.matches[0]!.t_s).toBeCloseTo(0.001, 6);
    await expect(call(client.send("rec.search", { rec: rec.rec, pattern: "D7 f=100 1010" }))).rejects.toThrow(/demo understands/);

    const snap = await call(client.send("rec.snapshot", { rec: rec.rec, t0_s: 0, t1_s: 0.02 }));
    const { results } = await call(client.send("capture.measure", { capture: snap.capture, channel: 7, measurements: ["frequency"] }));
    expect(results[0] && "value" in results[0] ? results[0].value : 0).toBeCloseTo(1000, 3);
    await call(client.send("rec.stop", { rec: rec.rec }));
    client.close();
  });

  it("switches USB ports, respects protection, and takes devices with them", async () => {
    const client = connect();
    await flush();
    const events: string[] = [];
    client.events.on((e) => events.push(e.event));
    await expect(call(client.send("usb.power", { hub: "3-3", port: 4, on: false }))).rejects.toThrow(/feeds hub/);
    await call(client.send("usb.power", { hub: "3-3.4", port: 1, on: false }));
    expect(events).toContain("device.removed");
    const { devices } = await call(client.send("device.list", {}));
    expect(devices.map((d) => d.id)).toEqual([SCOPE_ID]);
    await call(client.send("usb.power", { hub: "3-3.4", port: 1, on: true }));
    expect(events).toContain("device.added");
    client.close();
  });

  it("answers on a serial terminal", async () => {
    const client = connect();
    await flush();
    let output = "";
    client.termFrames.on((f) => {
      output += new TextDecoder().decode(f.payload);
    });
    const { sources } = await call(client.send("term.sources", {}));
    const serial = sources.find((s) => s.kind === "serial")!;
    const { session } = await call(client.send("term.open", { source: serial.source }));
    await flush();
    await call(client.send("term.write", { session, data: "temp\n" }));
    await flush();
    expect(output).toContain("23.4 C");
    client.close();
  });

  it("keeps an MCP log that grows while someone watches", async () => {
    const client = connect();
    await flush();
    const first = await call(client.send("mcp.log", {}));
    expect(first.entries.some((e) => e.tool === "survey_channels")).toBe(true);
    const withImage = first.entries.find((e) => e.images.length > 0)!;
    const image = await call(client.send("mcp.image", { seq: withImage.seq, index: 0 }));
    expect(image.mime_type).toBe("image/svg+xml");
    const events: string[] = [];
    client.events.on((e) => events.push(e.event));
    await flush(4100);
    expect(events).toContain("mcp.activity");
    const later = await call(client.send("mcp.log", { after: first.last_seq }));
    expect(later.entries.length).toBeGreaterThan(0);
    client.close();
  });

  it("shows traffic on the very first capture at the UI's defaults", async () => {
    const client = connect();
    await flush();
    const { frames } = await captureLogic(client, 24_000_000, 1024);
    const bytes = new Uint8Array(frames[0]!.payload.buffer, frames[0]!.payload.byteOffset, 1024);
    const uart = new Set(bytes.map((b) => b & 1));
    expect(uart.size).toBe(2);
    client.close();
  });

  it("ends a capture when its device loses power, and frees the device for a new one", async () => {
    const client = connect();
    await flush();
    const states: string[] = [];
    client.events.on((e) => {
      if (e.event === "acq.state") states.push(e.state);
    });
    await call(client.send("acq.configure", { device: SCOPE_ID, cfg: { rate_hz: 1_000_000, channels: [0], samples: 1000, ranges_v: [5], trigger: { kind: "edge", channel: 0, slope: "rising", level: 4.5 } } }));
    await call(client.send("acq.start", { device: SCOPE_ID }));
    expect(states.at(-1)).toBe("armed");
    await call(client.send("usb.power", { hub: "3-3.4", port: 2, on: false }));
    expect(states.at(-1)).toBe("stopped");
    await call(client.send("usb.power", { hub: "3-3.4", port: 2, on: true }));
    await flush(1000);
    await call(client.send("acq.configure", { device: SCOPE_ID, cfg: { rate_hz: 1_000_000, channels: [0], samples: 100, ranges_v: [5] } }));
    await expect(call(client.send("acq.start", { device: SCOPE_ID }))).resolves.toBeTruthy();
    client.close();
  });

  it("finds every analog crossing, and gives up on a level the signal never reaches", async () => {
    const client = connect();
    await flush();
    const rec = await call(client.send("rec.start", { device: SCOPE_ID, cfg: { rate_hz: 1_000_000, channels: [1], samples: 0, ranges_v: [5] } }));
    await flush(1000);
    const found = await call(client.send("rec.search", { rec: rec.rec, pattern: "CH2 rising 0V", max_matches: 3 }));
    expect(found.matches[1]!.t_s - found.matches[0]!.t_s).toBeCloseTo(0.0001, 5);
    expect(found.matches[2]!.t_s - found.matches[1]!.t_s).toBeCloseTo(0.0001, 5);
    const never = await call(client.send("rec.search", { rec: rec.rec, pattern: "CH2 rising 4.5V" }));
    expect(never.matches).toEqual([]);
    await call(client.send("rec.stop", { rec: rec.rec }));
    client.close();
  });

  it("refuses decoder settings the wires do not match, rather than decoding anyway", async () => {
    const client = connect();
    await flush();
    const { started } = await captureLogic(client, 8_000_000, 160_000);
    const uart = await call(client.send("capture.decode", { capture: started.capture, decoder: "uart", channels: [{ role: "rx", channel: 0 }], options: [{ id: "data_bits", value: 5 }] }));
    expect(uart.annotations).toEqual([]);
    expect(uart.note).toContain("8 data bits");
    const spi = await call(client.send("capture.decode", { capture: started.capture, decoder: "spi", channels: [{ role: "clk", channel: 4 }, { role: "mosi", channel: 5 }, { role: "miso", channel: 6 }, { role: "cs", channel: 3 }], options: [{ id: "mode", value: "mode3" }] }));
    expect(spi.annotations).toEqual([]);
    expect(spi.note).toContain("mode 0");
    client.close();
  });

  it("measures a long slow capture without aliasing", async () => {
    const client = connect();
    await flush();
    await call(client.send("acq.configure", { device: SCOPE_ID, cfg: { rate_hz: 100_000, channels: [1], samples: 1_000_000, ranges_v: [2.5] } }));
    const started = await call(client.send("acq.start", { device: SCOPE_ID }));
    const { results } = await call(client.send("capture.measure", { capture: started.capture, channel: 1, measurements: ["vpp", "frequency"] }));
    const vpp = results.find((r) => r.kind === "vpp");
    const freq = results.find((r) => r.kind === "frequency");
    expect(vpp && "value" in vpp ? vpp.value : 0).toBeGreaterThan(2.5);
    expect(freq && "value" in freq ? freq.value : 0).toBeCloseTo(10_000, -1);
    client.close();
  });

  it("does not draw a sine as a line when zoomed out", () => {
    const cal = { channel: 1, scale: 10 / 255, offset: -5, unit: "V", range_v: 5 };
    // 1 MS/s, 1.2 s, 1000 columns: buckets exactly twelve sine periods long.
    const view = buildAnalogView([1], [cal], 1_000_000, 0, 1_200_000, 1000);
    expect(view.kind).toBe(4);
    const volts = (code: number): number => code * cal.scale + cal.offset;
    expect(volts(view.payload[1]!) - volts(view.payload[0]!)).toBeGreaterThan(2.5);
  });

  it("holds a serial session while its device is away, and reopens it", async () => {
    const client = connect();
    await flush();
    const states: string[] = [];
    client.events.on((e) => {
      if (e.event === "term.state") states.push(e.state);
    });
    let frames = 0;
    client.termFrames.on(() => {
      frames += 1;
    });
    const { session } = await call(client.send("term.open", { source: "serial:/dev/ttyUSB0" }));
    await flush(2500);
    expect(frames).toBeGreaterThan(0);
    await call(client.send("usb.power", { hub: "3-3", port: 2, on: false }));
    expect(states).toEqual(["waiting"]);
    frames = 0;
    await flush(2500);
    expect(frames).toBe(0);
    await expect(call(client.send("term.write", { session, data: "x" }))).rejects.toThrow(/left the bus/);
    await call(client.send("usb.power", { hub: "3-3", port: 2, on: true }));
    expect(states).toEqual(["waiting", "open"]);
    await flush(1500);
    expect(frames).toBeGreaterThan(0);
    client.close();
  });

  it("answers every request the client can send, or refuses in words", async () => {
    const client = connect();
    await flush();
    await expect(call(client.send("nonsense" as "device.list", {}))).rejects.toThrow(/not part of the demo/);
    client.close();
  });
});

describe("the scene", () => {
  const scene = new LogicScene();

  it("repeats seamlessly", () => {
    for (let channel = 0; channel < 8; channel++) {
      expect(scene.levelAt(channel, PERIOD_S * 3 + 1e-9)).toBe(scene.levelAt(channel, 1e-9));
    }
  });

  it("alternates edge directions on every line", () => {
    for (let channel = 0; channel < 8; channel++) {
      let last: boolean | null = null;
      for (const e of scene.edgesBetween(channel, 0, PERIOD_S * 2)) {
        if (last !== null) expect(e.rising).toBe(!last);
        last = e.rising;
      }
    }
  });
});

describe("a sample window starting exactly on an edge", () => {
  it("does not invert the line", () => {
    const scene = new LogicScene();
    // D7 rises at the very start of a period and stays high for 25 % of each millisecond.
    const out = new Uint8Array(1000);
    scene.fillPacked(out, 1000, PERIOD_S * 5, 1_000_000, [WIRING.clock]);
    const high = out.reduce((n, byte) => n + ((byte >> WIRING.clock) & 1), 0);
    expect(high).toBe(250);
    expect((out[0]! >> WIRING.clock) & 1).toBe(1);
  });
});
