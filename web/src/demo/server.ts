import type {
  AcqConfig,
  CapabilityInfo,
  ChannelCalibration,
  ChannelMapping,
  DecodedAnnotation,
  DecoderId,
  DeviceInfo,
  Event as ServerEvent,
  McpLogEntry,
  McpLoggingState,
  MeasurementKind,
  MeasurementResult,
  OptionSetting,
  PatternMatch,
  RecordingSummary,
  Request as WireRequest,
  TerminalSourceInfo,
} from "../proto/types";
import type { SocketLike } from "../ws/socket";
import { pushLeb128, sampleFrame, termFrame, viewFrame } from "./frames";
import { MCP_SCRIPT, waveformSvg, type ScriptedCall } from "./mcp-script";
import { I2C_HZ, LogicScene, PERIOD_S, SPI_HZ, UART_BAUD, WIRING, analogVolts, noiseCode, sampleAtOrAfter } from "./scene";
import { ESP_ID, LOGIC_ID, SCOPE_ID, UART_BRIDGE_ID, UsbBench } from "./usb-bench";

/**
 * A complete AWIC server, in the browser, for the demo: it speaks the wire
 * protocol of SPECIFICATION.md §3 through a {@link SocketLike}, so `WsClient`
 * and every module above it run unmodified. Results are computed from
 * `scene.ts`, not canned, so a measurement or a decode agrees with the trace
 * on screen. What it cannot do is listed by what it refuses: the pattern
 * language is limited to edges, and rendered images do not exist.
 */

class DemoError extends Error {
  constructor(readonly code: string, msg: string) {
    super(msg);
  }
}

const LOGIC_RATES = [20000, 25000, 50000, 100000, 200000, 250000, 500000, 1000000, 2000000, 3000000, 4000000, 6000000, 8000000, 12000000, 16000000, 24000000];
const SCOPE_RATES = [48000000, 30000000, 24000000, 16000000, 8000000, 4000000, 1000000, 500000, 200000, 100000];
const SCOPE_RANGES = [5, 2.5, 1.25, 0.5];
/** A browser tab holds what it receives; the real server's limit is its capture store. */
const MAX_CAPTURE_SAMPLES = 4_000_000;
const CHUNK_SAMPLES = 65_536;
const MAX_ANNOTATIONS = 20_000;
const MAX_LOG_ENTRIES = 300;

const DEVICES: Readonly<Record<string, { label: string; vid: number; pid: number; capability: CapabilityInfo }>> = {
  [LOGIC_ID]: {
    label: "fx2lafw 8-channel logic analyzer",
    vid: 0x0925,
    pid: 0x3881,
    capability: { kind: "digital", channels: 8, max_rate_hz: 24000000, rates_hz: LOGIC_RATES },
  },
  [SCOPE_ID]: {
    label: "FX2 2-channel oscilloscope",
    vid: 0x1d50,
    pid: 0x608e,
    capability: { kind: "analog", channels: 2, bits: 8, rates_hz: SCOPE_RATES, ranges_v: SCOPE_RANGES },
  },
};

interface Capture {
  readonly device: string;
  readonly analog: boolean;
  readonly rate: number;
  readonly channels: readonly number[];
  readonly startS: number;
  readonly samples: number;
  readonly calibration: readonly ChannelCalibration[];
}

interface Recording {
  readonly rec: number;
  readonly device: string;
  readonly analog: boolean;
  readonly rate: number;
  readonly channels: readonly number[];
  readonly calibration: readonly ChannelCalibration[];
  readonly startedMs: number;
  stoppedMs: number | null;
  state: "recording" | "stopped" | "error";
  msg?: string;
}

interface Configured {
  readonly cfg: AcqConfig;
  readonly calibration: ChannelCalibration[];
}

function nearest(values: readonly number[], wanted: number): number {
  return values.reduce((best, v) => (Math.abs(v - wanted) < Math.abs(best - wanted) ? v : best), values[0]!);
}

function calibrationFor(channel: number, analog: boolean, rangeV: number): ChannelCalibration {
  if (!analog) return { channel, scale: 1, offset: 0, unit: "" };
  return { channel, scale: (2 * rangeV) / 255, offset: -rangeV, unit: "V", range_v: rangeV };
}

function codeFor(volts: number, cal: ChannelCalibration, sample: number): number {
  const code = Math.round((volts - cal.offset) / cal.scale) + noiseCode(sample, cal.channel);
  return Math.max(0, Math.min(255, code));
}

function median(values: readonly number[]): number {
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[sorted.length >> 1] ?? 0;
}


export class DemoServer {
  readonly scene = new LogicScene();
  readonly usb = new UsbBench();
  private readonly captures = new Map<string, Capture>();
  private readonly recordings = new Map<number, Recording>();
  private readonly busy = new Map<string, string>();
  private readonly connections = new Set<DemoSocket>();
  private readonly deviceStates = new Map<string, "ready" | "flashing">();
  private readonly logLines: string[] = [];
  private readonly mcpEntries: McpLogEntry[] = [];
  private mcpEnabled = true;
  private mcpImages = true;
  private mcpDropped = 0;
  private mcpSeq = 0;
  private mcpStep = 0;
  private mcpToolCalls = 0;
  private mcpErrors = 0;
  private agentTimer: ReturnType<typeof setTimeout> | null = null;
  private readonly sessionStartMs: number;
  /** Scene time counts from here: seconds since 1970 in a double cannot resolve one sample at 24 MS/s. */
  private readonly bootMs: number;
  private captureCount = 0;
  private nextStream = 1;
  private nextRec = 1;
  private nextView = 1;
  private nextSession = 1;

  constructor(private readonly now: () => number = () => Date.now()) {
    this.bootMs = this.now();
    this.sessionStartMs = this.bootMs - 180_000;
    this.seedMcpLog();
  }

  connect(socket: DemoSocket): void {
    this.connections.add(socket);
    this.scheduleAgent();
  }

  disconnect(socket: DemoSocket): void {
    this.connections.delete(socket);
    if (this.connections.size === 0 && this.agentTimer !== null) {
      clearTimeout(this.agentTimer);
      this.agentTimer = null;
    }
  }

  emit(event: Omit<ServerEvent, "v">): void {
    const text = JSON.stringify({ v: 1, ...event });
    for (const socket of this.connections) socket.deliver(text);
  }

  // ---- log -------------------------------------------------------------

  log(line: string): void {
    const stamp = new Date(this.now()).toISOString();
    const text = `${stamp} ${line}\r\n`;
    this.logLines.push(text);
    if (this.logLines.length > 200) this.logLines.shift();
    for (const socket of this.connections) socket.deliverLog(text);
  }

  logHistory(): string {
    return this.logLines.join("");
  }

  // ---- devices ---------------------------------------------------------

  devices(): DeviceInfo[] {
    return Object.keys(DEVICES).filter((id) => this.usb.isOn(id)).map((id) => this.deviceInfo(id));
  }

  private deviceInfo(id: string): DeviceInfo {
    const d = DEVICES[id]!;
    return { id, vid: d.vid, pid: d.pid, label: d.label, state: this.deviceStates.get(id) ?? "ready", capabilities: [d.capability] };
  }

  terminalSources(): TerminalSourceInfo[] {
    const serial: TerminalSourceInfo[] = [];
    if (this.usb.isOn(ESP_ID)) serial.push({ source: "serial:/dev/ttyACM0", kind: "serial", label: "ESP32-C6 dev board (/dev/ttyACM0)", device: ESP_ID, available: true });
    if (this.usb.isOn(UART_BRIDGE_ID)) serial.push({ source: "serial:/dev/ttyUSB0", kind: "serial", label: "CP2102 UART bridge (/dev/ttyUSB0)", device: UART_BRIDGE_ID, available: true });
    return [
      ...serial,
      { source: "log", kind: "log", label: "Server log", available: true },
      { source: "scpi", kind: "scpi", label: "SCPI console", available: true },
      { source: "decode", kind: "decode", label: "Decode a recording…", available: true },
    ];
  }

  setPortPower(hub: string, port: number, on: boolean): void {
    const info = this.usb.port(hub, port);
    if (!info) throw new DemoError("not_found", `no port ${port} on hub ${hub}`);
    if (!info.power_switchable) throw new DemoError("not_switchable", `hub ${hub} lacks per-port power switching`);
    if (info.protected) throw new DemoError("port_protected", info.protection ?? "port is protected");
    const before = this.devices().map((d) => d.id);
    const hadSources = this.terminalSources().length;
    this.usb.setPower(hub, port, on);
    this.log(`usb.power hub=${hub} port=${port} on=${on} origin=web`);
    const deviceId = this.usb.deviceIdAt(hub, port);
    this.emit({ event: "usb.changed", device: deviceId ?? `${hub}.${port}`, change: on ? "added" : "removed" } as Omit<ServerEvent, "v">);
    for (const id of before) {
      if (!this.usb.isOn(id)) {
        this.stopEverythingOn(id);
        this.emit({ event: "device.removed", device: id } as Omit<ServerEvent, "v">);
      }
    }
    if (on) {
      for (const id of Object.keys(DEVICES)) {
        if (this.usb.isOn(id) && !before.includes(id)) this.reenumerate(id);
      }
    }
    if (this.terminalSources().length !== hadSources) {
      this.emit({ event: "term.sources" } as Omit<ServerEvent, "v">);
      for (const socket of this.connections) socket.onSourcesChanged();
    }
  }

  /** A replugged instrument takes its firmware again before it is usable, as the real ones do. */
  private reenumerate(id: string): void {
    this.deviceStates.set(id, "flashing");
    this.emit({ event: "device.added", device: this.deviceInfo(id) } as Omit<ServerEvent, "v">);
    setTimeout(() => {
      this.deviceStates.delete(id);
      if (this.usb.isOn(id)) this.emit({ event: "device.state", device: id, state: "ready" } as Omit<ServerEvent, "v">);
    }, 900);
  }

  private stopEverythingOn(device: string): void {
    for (const rec of this.recordings.values()) {
      if (rec.device === device && rec.state === "recording") {
        rec.state = "error";
        rec.msg = "the device left the bus";
        rec.stoppedMs = this.now();
        this.emit({ event: "rec.state", rec: rec.rec, state: "error", msg: rec.msg } as Omit<ServerEvent, "v">);
      }
    }
    this.busy.delete(device);
  }

  // ---- acquisition -----------------------------------------------------

  configure(device: string, cfg: AcqConfig): Configured {
    const spec = DEVICES[device];
    if (!spec || !this.usb.isOn(device)) throw new DemoError("device_not_found", `no device ${device}`);
    const cap = spec.capability;
    if (cap.kind === "power_supply") throw new DemoError("unsupported", "not an acquisition device");
    const analog = cap.kind === "analog";
    const rate = nearest(cap.rates_hz, cfg.rate_hz);
    const channels = cfg.channels.filter((c) => c >= 0 && c < cap.channels);
    if (channels.length === 0) throw new DemoError("invalid_config", "select at least one channel");
    const rangesV = channels.map((_, i) => nearest(SCOPE_RANGES, cfg.ranges_v?.[i] ?? 5));
    const effective: AcqConfig = {
      ...cfg,
      rate_hz: rate,
      channels,
      samples: Math.min(cfg.samples, MAX_CAPTURE_SAMPLES),
      ...(analog ? { ranges_v: rangesV } : {}),
    };
    if (analog && cfg.trigger && cfg.trigger.level === undefined) {
      throw new DemoError("invalid_config", `an analog trigger needs a level: channel ${cfg.trigger.channel}`);
    }
    return { cfg: effective, calibration: channels.map((c, i) => calibrationFor(c, analog, rangesV[i]!)) };
  }

  claim(device: string, by: string): void {
    const holder = this.busy.get(device);
    if (holder) throw new DemoError("device_busy", `${device} is busy: held by ${holder}`);
    this.busy.set(device, by);
  }

  release(device: string): void {
    this.busy.delete(device);
  }

  /** Where in scene time a capture starts. Untriggered ones align to a burst, so the first look always shows traffic. */
  captureStart(device: string, cfg: AcqConfig): number | null {
    const wall = Math.floor((this.now() - this.bootMs) / 1000 / PERIOD_S) * PERIOD_S;
    const span = cfg.samples / cfg.rate_hz;
    const trigger = cfg.trigger;
    if (!trigger) return wall;
    const searchFrom = wall + span / 2;
    const found = DEVICES[device]?.capability.kind === "analog"
      ? this.findAnalogCrossing(trigger.channel, trigger.slope, trigger.level ?? 0, searchFrom, searchFrom + 0.01)
      : this.findEdge(trigger.channel, trigger.slope, searchFrom, searchFrom + PERIOD_S * 2);
    return found === null ? null : found - span / 2;
  }

  private findEdge(channel: number, slope: "rising" | "falling", t0: number, t1: number): number | null {
    for (const edge of this.scene.edgesBetween(channel, t0, t1)) {
      if (edge.rising === (slope === "rising")) return edge.t;
    }
    return null;
  }

  private findAnalogCrossing(channel: number, slope: "rising" | "falling", level: number, t0: number, t1: number): number | null {
    const step = 2e-7;
    let prev = analogVolts(channel, t0);
    for (let t = t0 + step; t < t1; t += step) {
      const v = analogVolts(channel, t);
      if (slope === "rising" ? prev < level && v >= level : prev > level && v <= level) return t;
      prev = v;
    }
    return null;
  }

  storeCapture(capture: Capture): string {
    this.captureCount += 1;
    const handle = `capture://demo-${this.captureCount.toString().padStart(4, "0")}`;
    this.captures.set(handle, capture);
    return handle;
  }

  capture(handle: string): Capture {
    const capture = this.captures.get(handle);
    if (!capture) throw new DemoError("capture_not_found", `no capture ${handle}`);
    return capture;
  }

  allocStream(): number {
    return this.nextStream++;
  }

  allocView(): number {
    return this.nextView++;
  }

  allocSession(): number {
    return this.nextSession++;
  }

  // ---- measurements ----------------------------------------------------

  measure(handle: string, channel: number, kinds: readonly MeasurementKind[]): MeasurementResult[] {
    const capture = this.capture(handle);
    const t0 = capture.startS;
    const t1 = t0 + capture.samples / capture.rate;
    return capture.analog
      ? measureAnalog(capture, channel, kinds)
      : measureDigital(this.scene, channel, t0, t1, kinds);
  }

  // ---- decoding --------------------------------------------------------

  decode(
    decoder: DecoderId,
    mapping: readonly ChannelMapping[],
    options: readonly OptionSetting[],
    source: { analog: boolean; rate: number; channels: readonly number[] },
    t0: number,
    t1: number,
    sampleOrigin: number,
  ): { annotations: DecodedAnnotation[]; truncated: boolean; omitted: number; detected_baud?: number; note?: string } {
    const refusal = decodeRefusal(decoder, mapping, options, source);
    if (refusal) return { annotations: [], truncated: false, omitted: 0, note: refusal };
    const annotations: DecodedAnnotation[] = [];
    let omitted = 0;
    for (const a of this.scene.annotationsBetween(decoder, t0, t1)) {
      if (annotations.length >= MAX_ANNOTATIONS) {
        omitted += 1;
        continue;
      }
      annotations.push({
        start_sample: Math.round((a.t0 - sampleOrigin) * source.rate),
        end_sample: Math.round((a.t1 - sampleOrigin) * source.rate),
        level: a.level,
        kind: a.kind,
        text: a.text,
      });
    }
    const auto = decoder === "uart" && options.find((o) => o.id === "baud")?.value === 0;
    return { annotations, truncated: omitted > 0, omitted, ...(auto ? { detected_baud: UART_BAUD } : {}) };
  }

  // ---- recordings ------------------------------------------------------

  startRecording(device: string, cfg: AcqConfig): Recording {
    const configured = this.configure(device, { ...cfg, samples: 0, continuous: true, trigger: null });
    this.claim(device, "a recording");
    const rec: Recording = {
      rec: this.nextRec++,
      device,
      analog: DEVICES[device]!.capability.kind === "analog",
      rate: configured.cfg.rate_hz,
      channels: configured.cfg.channels,
      calibration: configured.calibration,
      startedMs: this.now(),
      stoppedMs: null,
      state: "recording",
    };
    this.recordings.set(rec.rec, rec);
    this.log(`rec.start rec=${rec.rec} device=${device} rate_hz=${rec.rate} origin=web`);
    this.emit({ event: "rec.state", rec: rec.rec, state: "recording" } as Omit<ServerEvent, "v">);
    return rec;
  }

  recording(id: number): Recording {
    const rec = this.recordings.get(id);
    if (!rec) throw new DemoError("recording_not_found", `no recording ${id}`);
    return rec;
  }

  recordingHead(rec: Recording): number {
    const end = rec.stoppedMs ?? this.now();
    return Math.floor(((end - rec.startedMs) / 1000) * rec.rate);
  }

  stopRecording(id: number): void {
    const rec = this.recording(id);
    if (rec.state === "recording") {
      rec.state = "stopped";
      rec.stoppedMs = this.now();
      this.release(rec.device);
      this.log(`rec.stop rec=${id} origin=web`);
      this.emit({ event: "rec.state", rec: id, state: "stopped" } as Omit<ServerEvent, "v">);
    }
  }

  deleteRecording(id: number): void {
    const rec = this.recording(id);
    if (rec.state === "recording") this.release(rec.device);
    this.recordings.delete(id);
  }

  listRecordings(): { recordings: RecordingSummary[]; budget_mb: number; used_bytes: number } {
    const recordings = [...this.recordings.values()].map((rec): RecordingSummary => {
      const head = this.recordingHead(rec);
      const raw = head * (rec.analog ? rec.channels.length : 1);
      const bytes = Math.round(raw / (rec.analog ? 2.5 : 80));
      return {
        rec: rec.rec,
        device: rec.device,
        kind: rec.analog ? "analog" : "digital",
        rate_hz: rec.rate,
        channels: rec.channels.map((channel, i) => ({
          channel,
          name: rec.analog ? `CH${channel + 1}` : `D${channel}`,
          calibration: rec.calibration[i]!,
        })),
        first_sample: 0,
        head_sample: head,
        bytes,
        ratio: raw / Math.max(1, bytes),
        state: rec.state,
        ...(rec.msg ? { msg: rec.msg } : {}),
      };
    });
    return { recordings, budget_mb: 256, used_bytes: recordings.reduce((sum, r) => sum + r.bytes, 0) };
  }

  /** The demo's pattern language: one channel and an edge. */
  searchMatches(rec: Recording, pattern: string, t0: number, t1: number, limit: number): PatternMatch[] {
    const parsed = parsePattern(pattern, rec);
    const matches: PatternMatch[] = [];
    if (parsed.analog) {
      let from = t0;
      while (matches.length < limit && from < t1) {
        const t = this.findAnalogCrossing(parsed.channel, parsed.slope ?? "rising", parsed.level, from, t1);
        if (t === null) break;
        matches.push({ t_s: t, sample: Math.round(t * rec.rate) });
        from = t + 1 / 10_000;
      }
      return matches;
    }
    for (const edge of this.scene.edgesBetween(parsed.channel, t0, t1)) {
      if (parsed.slope && edge.rising !== (parsed.slope === "rising")) continue;
      matches.push({ t_s: edge.t, sample: Math.round(edge.t * rec.rate) });
      if (matches.length >= limit) break;
    }
    return matches;
  }

  // ---- MCP -------------------------------------------------------------

  mcpLogging(): McpLoggingState {
    const used = this.mcpEntries.reduce((sum, e) => sum + (e.request?.length ?? 0) + (e.response?.length ?? 0) + 200, 0);
    return { enabled: this.mcpEnabled, images: this.mcpImages, memory_used_bytes: used, memory_limit_bytes: 16777216, entries: this.mcpEntries.length, dropped: this.mcpDropped };
  }

  setMcpLogging(enabled?: boolean | null, images?: boolean | null, clear?: boolean | null): McpLoggingState {
    if (enabled !== undefined && enabled !== null) this.mcpEnabled = enabled;
    if (images !== undefined && images !== null) this.mcpImages = images;
    if (clear) this.mcpEntries.length = 0;
    return this.mcpLogging();
  }

  mcpLog(after: number, limit: number): McpLogEntry[] {
    return this.mcpEntries.filter((e) => e.seq > after).slice(0, limit);
  }

  mcpLastSeq(): number {
    return this.mcpSeq;
  }

  mcpImage(seq: number, index: number): { mime_type: string; data_b64: string } {
    const entry = this.mcpEntries.find((e) => e.seq === seq);
    if (!entry || !entry.images[index]) throw new DemoError("not_found", `no image ${index} on entry ${seq}`);
    return { mime_type: "image/svg+xml", data_b64: btoa(waveformSvg()) };
  }

  mcpStatus() {
    const now = this.now();
    return {
      endpoint_path: "/mcp",
      bind: "127.0.0.1:8080",
      auth_required: false,
      server_name: "awic (simulated)",
      server_version: "demo",
      protocol_version: "2025-06-18",
      tool_count: 27,
      max_sessions: 32,
      sessions: [
        {
          id: "demo-session",
          client: "Claude Code",
          client_version: "2.1",
          protocol_version: "2025-06-18",
          started_ms: this.sessionStartMs,
          last_seen_ms: this.mcpEntries.at(-1)?.t_ms ?? now,
          requests: this.mcpToolCalls + 1,
          tool_calls: this.mcpToolCalls,
          errors: this.mcpErrors,
          last_tool: this.mcpEntries.at(-1)?.tool ?? null,
          active: true,
        },
      ],
      logging: this.mcpLogging(),
    };
  }

  private seedMcpLog(): void {
    this.pushMcpEntry({
      t_ms: this.sessionStartMs,
      session: "demo-session",
      client: "Claude Code",
      kind: "session_start",
      method: "initialize",
      duration_ms: 1,
      is_error: false,
      summary: "Claude Code 2.1, MCP 2025-06-18",
      request: JSON.stringify({ protocolVersion: "2025-06-18", clientInfo: { name: "Claude Code", version: "2.1" } }, null, 2),
      response: JSON.stringify({ protocolVersion: "2025-06-18", serverInfo: { name: "awic (simulated)", version: "demo" } }, null, 2),
      images: [],
    });
    const seeded = MCP_SCRIPT.length;
    for (let i = 0; i < seeded; i++) {
      this.pushScripted(MCP_SCRIPT[i]!, this.sessionStartMs + (i + 1) * 15_000);
    }
  }

  private pushScripted(call: ScriptedCall, t: number): void {
    this.mcpToolCalls += 1;
    if (call.error) this.mcpErrors += 1;
    const svgBytes = call.image ? waveformSvg().length : 0;
    this.pushMcpEntry({
      t_ms: t,
      session: "demo-session",
      client: "Claude Code",
      kind: "request",
      method: "tools/call",
      tool: call.tool,
      duration_ms: call.durationMs,
      is_error: call.error ?? false,
      summary: call.summary,
      request: JSON.stringify({ name: call.tool, arguments: call.args }, null, 2),
      response: JSON.stringify(call.result, null, 2),
      images: call.image ? [{ mime_type: "image/svg+xml", bytes: svgBytes, kept: this.mcpImages }] : [],
    });
  }

  private pushMcpEntry(entry: Omit<McpLogEntry, "seq">): void {
    if (!this.mcpEnabled) return;
    this.mcpSeq += 1;
    this.mcpEntries.push({ ...entry, seq: this.mcpSeq });
    while (this.mcpEntries.length > MAX_LOG_ENTRIES) {
      this.mcpEntries.shift();
      this.mcpDropped += 1;
    }
  }

  private scheduleAgent(): void {
    if (this.agentTimer !== null || this.connections.size === 0) return;
    this.agentTimer = setTimeout(() => {
      this.agentTimer = null;
      const call = MCP_SCRIPT[this.mcpStep % MCP_SCRIPT.length]!;
      this.mcpStep += 1;
      this.pushScripted(call, this.now());
      this.log(`${call.log} origin=mcp:claude-code`);
      this.emit({ event: "mcp.activity", seq: this.mcpSeq, sessions_changed: false } as Omit<ServerEvent, "v">);
      this.scheduleAgent();
    }, 4000);
  }
}

// ---- pure helpers ------------------------------------------------------

function decodeRefusal(
  decoder: DecoderId,
  mapping: readonly ChannelMapping[],
  options: readonly OptionSetting[],
  source: { analog: boolean; rate: number; channels: readonly number[] },
): string | null {
  if (source.analog) return "The demo's protocol traffic is on the logic analyzer: pick it in the device list and capture again.";
  const role = (name: ChannelMapping["role"]): number | undefined => mapping.find((m) => m.role === name)?.channel;
  const wrong = (expected: string): string => `No ${decoder.toUpperCase()} traffic on those channels. On this demo bench it is on ${expected}.`;
  const enoughRate = (bitHz: number): string | null =>
    source.rate >= bitHz * 8 ? null : `The sample rate is too low to read this bus: use at least ${(bitHz * 8) / 1e6} MS/s.`;
  const has = (...channels: number[]): boolean => channels.every((c) => source.channels.includes(c));
  switch (decoder) {
    case "uart": {
      if (role("rx") !== WIRING.uart.rx) return wrong("D0");
      if (!has(WIRING.uart.rx)) return "D0 was not captured: enable it and capture again.";
      const baud = options.find((o) => o.id === "baud")?.value;
      if (typeof baud === "number" && baud !== 0 && Math.abs(baud - UART_BAUD) / UART_BAUD > 0.03) {
        return `Framing errors at ${baud} baud: this line runs at ${UART_BAUD}. Set baud to 0 to detect it.`;
      }
      return enoughRate(UART_BAUD);
    }
    case "i2c":
      if (role("scl") !== WIRING.i2c.scl || role("sda") !== WIRING.i2c.sda) return wrong("SCL D1, SDA D2");
      if (!has(WIRING.i2c.scl, WIRING.i2c.sda)) return "D1 and D2 must both be captured.";
      return enoughRate(I2C_HZ);
    case "spi": {
      const { clk, mosi, miso, cs } = WIRING.spi;
      if (role("clk") !== clk || role("mosi") !== mosi || role("miso") !== miso || role("cs") !== cs) return wrong("CS D3, SCK D4, MOSI D5, MISO D6");
      if (!has(clk, mosi, miso, cs)) return "D3 to D6 must all be captured.";
      return enoughRate(SPI_HZ);
    }
    default:
      return `There is no ${decoder === "can" ? "CAN" : "1-Wire"} traffic on the demo bench.`;
  }
}

function digitalPulses(scene: LogicScene, channel: number, t0: number, t1: number): { high: number[]; low: number[]; rises: number[]; edges: number; highTime: number } {
  const high: number[] = [];
  const low: number[] = [];
  const rises: number[] = [];
  let edges = 0;
  let last: number | null = null;
  let level = scene.levelAt(channel, t0);
  let highTime = 0;
  let cursor = t0;
  for (const edge of scene.edgesBetween(channel, t0, t1)) {
    edges += 1;
    if (level) highTime += edge.t - cursor;
    cursor = edge.t;
    if (last !== null) (level ? high : low).push(edge.t - last);
    if (edge.rising) rises.push(edge.t);
    last = edge.t;
    level = edge.rising ? 1 : 0;
  }
  if (level) highTime += t1 - cursor;
  return { high, low, rises, edges, highTime };
}

function measureDigital(scene: LogicScene, channel: number, t0: number, t1: number, kinds: readonly MeasurementKind[]): MeasurementResult[] {
  const p = digitalPulses(scene, channel, t0, t1);
  const period = p.rises.length >= 2 ? (p.rises.at(-1)! - p.rises[0]!) / (p.rises.length - 1) : null;
  return kinds.map((kind): MeasurementResult => {
    const none = (why: string): MeasurementResult => ({ kind, unavailable: why });
    switch (kind) {
      case "frequency":
        return period ? { kind, value: 1 / period, unit: "Hz" } : none("fewer than two rising edges in this capture");
      case "period":
        return period ? { kind, value: period, unit: "s" } : none("fewer than two rising edges in this capture");
      case "duty":
        return { kind, value: (100 * p.highTime) / (t1 - t0), unit: "%" };
      case "edge_count":
        return { kind, value: p.edges, unit: "count" };
      case "pulse_width_high":
        return p.high.length ? { kind, value: median(p.high), unit: "s" } : none("no complete high pulse in this capture");
      case "pulse_width_low":
        return p.low.length ? { kind, value: median(p.low), unit: "s" } : none("no complete low pulse in this capture");
      case "vpp":
      case "vmin":
      case "vmax":
      case "vavg":
      case "vrms":
      case "rise_time":
      case "fall_time":
        return none(`this is an analog-only measurement; this capture is digital`);
      default:
        return none("not measured in the demo");
    }
  });
}

function measureAnalog(capture: Capture, channel: number, kinds: readonly MeasurementKind[]): MeasurementResult[] {
  const count = Math.min(capture.samples, 200_000);
  const step = capture.samples / count;
  const cal = capture.calibration.find((c) => c.channel === channel);
  if (!cal) return kinds.map((kind) => ({ kind, unavailable: "this channel was not captured" }));
  const volts: number[] = [];
  for (let i = 0; i < count; i++) {
    const sample = Math.floor(i * step);
    const code = codeFor(analogVolts(channel, capture.startS + sample / capture.rate), cal, sample);
    volts.push(code * cal.scale + cal.offset);
  }
  let min = Infinity;
  let max = -Infinity;
  for (const v of volts) {
    if (v < min) min = v;
    if (v > max) max = v;
  }
  const mid = (min + max) / 2;
  const rises: number[] = [];
  for (let i = 1; i < volts.length; i++) {
    if (volts[i - 1]! < mid && volts[i]! >= mid && (rises.length === 0 || i - rises.at(-1)! > 4)) rises.push(i);
  }
  const period = rises.length >= 2 ? (((rises.at(-1)! - rises[0]!) / (rises.length - 1)) * step) / capture.rate : null;
  let highCount = 0;
  let sum = 0;
  let squares = 0;
  for (const v of volts) {
    if (v >= mid) highCount += 1;
    sum += v;
    squares += v * v;
  }
  return kinds.map((kind): MeasurementResult => {
    switch (kind) {
      case "vpp":
        return { kind, value: max - min, unit: "V" };
      case "vmin":
        return { kind, value: min, unit: "V" };
      case "vmax":
        return { kind, value: max, unit: "V" };
      case "vavg":
        return { kind, value: sum / volts.length, unit: "V" };
      case "vrms":
        return { kind, value: Math.sqrt(squares / volts.length), unit: "V" };
      case "frequency":
        return period ? { kind, value: 1 / period, unit: "Hz" } : { kind, unavailable: "fewer than two rising crossings in this capture" };
      case "period":
        return period ? { kind, value: period, unit: "s" } : { kind, unavailable: "fewer than two rising crossings in this capture" };
      case "duty":
        return { kind, value: (100 * highCount) / volts.length, unit: "%" };
      case "edge_count":
        return { kind, value: rises.length * 2, unit: "count" };
      default:
        return { kind, unavailable: "not measured in the demo" };
    }
  });
}

interface ParsedPattern {
  readonly analog: boolean;
  readonly channel: number;
  readonly slope: "rising" | "falling" | null;
  readonly level: number;
}

function parsePattern(pattern: string, rec: Recording): ParsedPattern {
  const help = rec.analog
    ? `The demo understands an edge with a level, e.g. "CH1 rising 1V". The real server has the whole pattern language: pulses, bit sequences, clocked data.`
    : `The demo understands one channel and an edge, e.g. "D0 falling". The real server has the whole pattern language: pulses, bit sequences, clocked data.`;
  const invalid = (): DemoError => new DemoError("invalid_pattern", help);
  const tokens = pattern.trim().toLowerCase().split(/\s+/);
  if (rec.analog) {
    const m = /^ch(\d)$/.exec(tokens[0] ?? "");
    const level = /^(-?\d+(?:\.\d+)?)v$/.exec(tokens[2] ?? "");
    if (!m || !level || (tokens[1] !== "rising" && tokens[1] !== "falling") || tokens.length !== 3) throw invalid();
    const channel = Number(m[1]) - 1;
    if (!rec.channels.includes(channel)) throw invalid();
    return { analog: true, channel, slope: tokens[1], level: Number(level[1]) };
  }
  const m = /^d(\d)$/.exec(tokens[0] ?? "");
  const dir = tokens[1];
  if (!m || (dir !== "rising" && dir !== "falling" && dir !== "edge") || tokens.length !== 2) throw invalid();
  const channel = Number(m[1]);
  if (!rec.channels.includes(channel)) throw invalid();
  return { analog: false, channel, slope: dir === "edge" ? null : dir, level: 0 };
}

/** One view frame's payload for a window, chosen as SPECIFICATION.md §3.4 says the server chooses. */
export function buildDigitalView(
  scene: LogicScene,
  channels: readonly number[],
  rate: number,
  first: number,
  last: number,
  columns: number,
): { kind: 1 | 2; itemCount: number; firstSample: number; samplesPerItem: number; payload: Uint8Array } {
  const span = Math.max(1, last - first);
  const t0 = first / rate;
  const t1 = last / rate;
  const startLevels = channels.reduce((acc, c) => acc | (scene.levelAt(c, t0) << c), 0);
  // Past a few periods per column every active line toggled in every column:
  // no need to walk every edge in the window for that answer.
  const tooLong = t1 - t0 > PERIOD_S * columns;
  const toggles: Array<[number, number]> = [];
  if (!tooLong) {
    for (const c of channels) {
      for (const e of scene.edgesBetween(c, t0, t1)) toggles.push([Math.min(last - 1, sampleAtOrAfter(e.t * rate)), 1 << c]);
    }
    toggles.sort((a, b) => a[0] - b[0]);
  }
  if (!tooLong && toggles.length <= 4 * columns) {
    const out: number[] = [];
    let value = startLevels;
    let pos = first;
    let items = 0;
    let i = 0;
    while (i < toggles.length) {
      const at = toggles[i]![0];
      if (at > pos) {
        out.push(value);
        pushLeb128(out, at - pos);
        items += 1;
        pos = at;
      }
      while (i < toggles.length && toggles[i]![0] === at) {
        value ^= toggles[i]![1];
        i += 1;
      }
    }
    if (last > pos) {
      out.push(value);
      pushLeb128(out, last - pos);
      items += 1;
    }
    return { kind: 1, itemCount: items, firstSample: first, samplesPerItem: 1, payload: Uint8Array.from(out) };
  }

  const samplesPerItem = Math.max(1, Math.ceil(span / columns));
  const aligned = Math.floor(first / samplesPerItem) * samplesPerItem;
  const itemCount = Math.ceil((last - aligned) / samplesPerItem);
  const payload = new Uint8Array(itemCount * 2);
  if (tooLong) {
    let activeMask = 0;
    for (const c of channels) activeMask |= 1 << c;
    for (let i = 0; i < itemCount; i++) {
      payload[i * 2] = 0;
      payload[i * 2 + 1] = activeMask;
    }
    return { kind: 2, itemCount, firstSample: aligned, samplesPerItem, payload };
  }
  let value = startLevels;
  let next = 0;
  for (let i = 0; i < itemCount; i++) {
    const end = aligned + (i + 1) * samplesPerItem;
    let and = value;
    let or = value;
    while (next < toggles.length && toggles[next]![0] < end) {
      value ^= toggles[next]![1];
      and &= value;
      or |= value;
      next += 1;
    }
    payload[i * 2] = and;
    payload[i * 2 + 1] = or;
  }
  return { kind: 2, itemCount, firstSample: aligned, samplesPerItem, payload };
}

export function buildAnalogView(
  channels: readonly number[],
  calibration: readonly ChannelCalibration[],
  rate: number,
  first: number,
  last: number,
  columns: number,
): { kind: 3 | 4; itemCount: number; firstSample: number; samplesPerItem: number; payload: Uint8Array } {
  const span = Math.max(1, last - first);
  const n = channels.length;
  if (span <= 2 * columns) {
    const payload = new Uint8Array(span * n);
    for (let s = 0; s < span; s++) {
      for (let c = 0; c < n; c++) {
        const sample = first + s;
        payload[s * n + c] = codeFor(analogVolts(channels[c]!, sample / rate), calibration[c]!, sample);
      }
    }
    return { kind: 3, itemCount: span, firstSample: first, samplesPerItem: 1, payload };
  }
  const samplesPerItem = Math.ceil(span / columns);
  const aligned = Math.floor(first / samplesPerItem) * samplesPerItem;
  const itemCount = Math.ceil((last - aligned) / samplesPerItem);
  const payload = new Uint8Array(itemCount * 2 * n);
  const probes = Math.min(samplesPerItem, 12);
  for (let c = 0; c < n; c++) {
    for (let i = 0; i < itemCount; i++) {
      let min = 255;
      let max = 0;
      for (let p = 0; p < probes; p++) {
        const sample = aligned + i * samplesPerItem + Math.floor((p * samplesPerItem) / probes);
        const code = codeFor(analogVolts(channels[c]!, sample / rate), calibration[c]!, sample);
        if (code < min) min = code;
        if (code > max) max = code;
      }
      payload[(c * itemCount + i) * 2] = min;
      payload[(c * itemCount + i) * 2 + 1] = max;
    }
  }
  return { kind: 4, itemCount, firstSample: aligned, samplesPerItem, payload };
}

// ---- the connection ----------------------------------------------------

interface View {
  readonly id: number;
  readonly rec: Recording;
  columns: number;
  followS: number | null;
  t0: number;
  t1: number;
  sequence: number;
  timer: ReturnType<typeof setInterval> | null;
}

interface Session {
  readonly id: number;
  readonly source: string;
  sequence: number;
  timers: Array<ReturnType<typeof setInterval>>;
  input: string;
}

const ESP_BOOT = "\x1b[0mESP-ROM:esp32c6-20220919\r\nrst:0x1 (POWERON),boot:0x7c\r\n[awic-demo] firmware 1.4.2, sensor at I2C 0x48, flash JEDEC EF 40 18\r\n[awic-demo] type help for commands\r\nesp> ";
const NMEA_LINES = [
  "$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*47\r\n",
  "$GPVTG,054.7,T,034.4,M,005.5,N,010.2,K*48\r\n",
];

const ACK = {} as Record<symbol, never>;

export class DemoSocket implements SocketLike {
  onopen: (() => void) | null = null;
  onclose: ((code: number, reason: string) => void) | null = null;
  onerror: (() => void) | null = null;
  onmessage: ((data: string | ArrayBuffer) => void) | null = null;

  private closed = false;
  private readonly views = new Map<number, View>();
  private readonly sessions = new Map<number, Session>();
  private readonly streams = new Map<number, { device: string; stop: () => void }>();
  private readonly pendingConfig = new Map<string, Configured>();

  constructor(private readonly server: DemoServer) {
    setTimeout(() => {
      if (this.closed) return;
      server.connect(this);
      this.onopen?.();
    }, 60);
  }

  send(data: string | ArrayBuffer | ArrayBufferView<ArrayBuffer>): void {
    if (typeof data !== "string") return;
    let request: WireRequest;
    try {
      request = JSON.parse(data) as WireRequest;
    } catch {
      return;
    }
    void this.answer(request);
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    for (const view of this.views.values()) if (view.timer !== null) clearInterval(view.timer);
    for (const session of this.sessions.values()) session.timers.forEach(clearInterval);
    for (const stream of this.streams.values()) stream.stop();
    this.views.clear();
    this.sessions.clear();
    this.server.disconnect(this);
    setTimeout(() => this.onclose?.(1000, ""), 0);
  }

  deliver(data: string | ArrayBuffer): void {
    if (!this.closed) this.onmessage?.(data);
  }

  deliverLog(text: string): void {
    for (const session of this.sessions.values()) {
      if (session.source === "log") this.writeTerm(session, text);
    }
  }

  onSourcesChanged(): void {
    const sources = new Set(this.server.terminalSources().map((s) => s.source));
    for (const session of [...this.sessions.values()]) {
      if (session.source.startsWith("serial:") && !sources.has(session.source)) {
        this.deliver(JSON.stringify({ v: 1, event: "term.state", session: session.id, state: "waiting", msg: "the device left the bus" }));
      }
    }
  }

  private async answer(request: WireRequest): Promise<void> {
    await new Promise((resolve) => setTimeout(resolve, 8));
    if (this.closed) return;
    try {
      const result = await this.handle(request);
      this.deliver(JSON.stringify({ v: 1, id: request.id, ok: true, result }));
    } catch (err) {
      const error = err instanceof DemoError
        ? { code: err.code, msg: err.message }
        : { code: "internal", msg: err instanceof Error ? err.message : String(err) };
      this.deliver(JSON.stringify({ v: 1, id: request.id, ok: false, error }));
    }
  }

  private handle(req: WireRequest): unknown | Promise<unknown> {
    const server = this.server;
    switch (req.op) {
      case "device.list":
        return { devices: server.devices() };
      case "usb.tree":
        return { roots: server.usb.tree() };
      case "usb.power":
        server.setPortPower(req.hub, req.port, req.on);
        return { hub: req.hub, port: req.port, on: req.on };
      case "usb.protect": {
        server.usb.setChoice(req.hub, req.port, req.protected);
        const port = server.usb.port(req.hub, req.port);
        if (!port) throw new DemoError("not_found", `no port ${req.port} on hub ${req.hub}`);
        server.log(`usb.protect hub=${req.hub} port=${req.port} protected=${String(req.protected)} origin=web`);
        server.emit({ event: "usb.changed", device: `${req.hub}.${req.port}`, change: "protection" } as Omit<ServerEvent, "v">);
        return { hub: req.hub, port: req.port, protected: port.protected, reason: port.protection, user_choice: port.user_choice };
      }
      case "acq.configure": {
        const configured = server.configure(req.device, req.cfg);
        this.pendingConfig.set(req.device, configured);
        return { device: req.device, cfg: configured.cfg, calibration: configured.calibration };
      }
      case "acq.start":
        return this.startAcquisition(req.device);
      case "acq.stop":
        this.streams.get(req.stream)?.stop();
        return ACK;
      case "capture.measure":
        return { capture: req.capture, channel: req.channel, results: server.measure(req.capture, req.channel, req.measurements) };
      case "capture.decode": {
        const capture = server.capture(req.capture);
        const span = capture.samples / capture.rate;
        const t0 = capture.startS + (req.t0_s ?? 0);
        const t1 = capture.startS + (req.t1_s ?? span);
        const result = server.decode(req.decoder, req.channels, req.options, capture, t0, t1, capture.startS);
        return { capture: req.capture, decoder: req.decoder, ...result };
      }
      case "rec.start": {
        const rec = server.startRecording(req.device, req.cfg);
        return { rec: rec.rec, cfg: { rate_hz: rec.rate, channels: rec.channels, samples: 0, continuous: true }, calibration: rec.calibration };
      }
      case "rec.stop":
        server.stopRecording(req.rec);
        return ACK;
      case "rec.delete":
        server.deleteRecording(req.rec);
        return ACK;
      case "rec.list":
        return server.listRecordings();
      case "rec.view":
        return this.openView(req.rec, req.columns, req.follow_s ?? null, req.t0_s ?? null, req.t1_s ?? null);
      case "rec.view.update":
        this.updateView(req.view, req.columns ?? null, req.follow_s ?? null, req.t0_s ?? null, req.t1_s ?? null);
        return ACK;
      case "rec.view.close":
        this.closeView(req.view);
        return ACK;
      case "rec.decode": {
        const rec = server.recording(req.rec);
        const head = server.recordingHead(rec) / rec.rate;
        const t0 = Math.max(0, req.t0_s ?? 0);
        const t1 = Math.min(head, req.t1_s ?? head);
        const result = server.decode(req.decoder, req.channels, req.options, rec, t0, t1, 0);
        return { rec: req.rec, decoder: req.decoder, ...result, decoded: { t0_s: t0, t1_s: t1 } };
      }
      case "rec.search": {
        const rec = server.recording(req.rec);
        const head = server.recordingHead(rec) / rec.rate;
        const t0 = Math.max(0, req.t0_s ?? 0);
        const t1 = Math.min(head, req.t1_s ?? head);
        // No search walks more of the signal than a screenful of results needs.
        const limit = req.max_matches ?? 1000;
        const matches = server.searchMatches(rec, req.pattern, t0, t1, limit + 1);
        return { matches: matches.slice(0, limit), truncated: matches.length > limit, searched: { t0_s: t0, t1_s: t1 }, glitches: 0 };
      }
      case "rec.wait":
        return this.waitForPattern(req.rec, req.pattern, req.timeout_ms ?? 10_000);
      case "rec.snapshot": {
        const rec = server.recording(req.rec);
        const samples = Math.max(1, Math.round((req.t1_s - req.t0_s) * rec.rate));
        if (samples > MAX_CAPTURE_SAMPLES) throw new DemoError("snapshot_too_large", `the demo snapshots up to ${MAX_CAPTURE_SAMPLES} samples; narrow the range`);
        return { capture: server.storeCapture({ device: rec.device, analog: rec.analog, rate: rec.rate, channels: rec.channels, startS: req.t0_s, samples, calibration: rec.calibration }) };
      }
      case "decode.list":
        return { decoders: DECODERS };
      case "term.sources":
        return { sources: server.terminalSources() };
      case "term.open":
        return this.openTerminal(req.source, req.decode ?? null);
      case "term.write":
        this.writeToTerminal(req.session, req.data ?? (req.data_b64 ? atob(req.data_b64) : ""));
        return ACK;
      case "term.close": {
        const session = this.sessions.get(req.session);
        if (session) {
          session.timers.forEach(clearInterval);
          this.sessions.delete(req.session);
          this.deliver(JSON.stringify({ v: 1, event: "term.state", session: req.session, state: "closed" }));
        }
        return ACK;
      }
      case "mcp.status":
        return server.mcpStatus();
      case "mcp.log": {
        const entries = server.mcpLog(req.after ?? 0, req.limit ?? 500);
        return { entries, last_seq: server.mcpLastSeq(), logging: server.mcpLogging() };
      }
      case "mcp.image":
        return server.mcpImage(req.seq, req.index);
      case "mcp.logging":
        return server.setMcpLogging(req.enabled, req.images, req.clear);
      default:
        throw new DemoError("not_in_demo", `${req.op} is not part of the demo`);
    }
  }

  // ---- single captures -------------------------------------------------

  private startAcquisition(device: string): { device: string; stream: number; capture: string } {
    const server = this.server;
    const configured = this.pendingConfig.get(device);
    if (!configured) throw new DemoError("not_configured", `configure ${device} first`);
    server.claim(device, "a capture");
    const cfg = configured.cfg;
    const analog = DEVICES[device]!.capability.kind === "analog";
    const stream = server.allocStream();
    const startS = server.captureStart(device, cfg);
    const capture = server.storeCapture({
      device,
      analog,
      rate: cfg.rate_hz,
      channels: cfg.channels,
      startS: startS ?? 0,
      samples: cfg.samples,
      calibration: configured.calibration,
    });
    server.log(`acq.start device=${device} rate_hz=${cfg.rate_hz} samples=${cfg.samples} origin=web`);

    let stopped = false;
    const finish = (msg: string): void => {
      if (stopped) return;
      stopped = true;
      this.streams.delete(stream);
      server.release(device);
      this.deliver(JSON.stringify({ v: 1, event: "acq.state", stream, state: "stopped", msg }));
    };
    this.streams.set(stream, { device, stop: () => finish("stopped") });

    if (startS === null) {
      // The trigger can never fire (a level outside the signal): stay armed, as a real scope does.
      setTimeout(() => this.deliver(JSON.stringify({ v: 1, event: "acq.state", stream, state: "armed" })), 0);
      return { device, stream, capture };
    }
    setTimeout(() => {
      this.deliver(JSON.stringify({ v: 1, event: "acq.state", stream, state: "running" }));
      let sent = 0;
      let sequence = 0;
      const pump = (): void => {
        if (stopped || this.closed) return;
        const n = Math.min(CHUNK_SAMPLES, cfg.samples - sent);
        const t0 = startS + sent / cfg.rate_hz;
        const t0Ns = BigInt(Math.round((sent / cfg.rate_hz) * 1e9));
        let payload: Uint8Array;
        if (analog) {
          const width = cfg.channels.length;
          payload = new Uint8Array(n * width);
          for (let i = 0; i < n; i++) {
            for (let c = 0; c < width; c++) {
              const sample = sent + i;
              payload[i * width + c] = codeFor(analogVolts(cfg.channels[c]!, startS + sample / cfg.rate_hz), configured.calibration[c]!, sample);
            }
          }
        } else {
          payload = new Uint8Array(n);
          server.scene.fillPacked(payload, n, t0, cfg.rate_hz, cfg.channels);
        }
        this.deliver(sampleFrame({
          stream,
          sequence: sequence++,
          encoding: analog ? 0 : 2,
          channelCount: cfg.channels.length,
          sampleCount: n,
          t0Ns,
          payload,
        }));
        sent += n;
        if (sent >= cfg.samples) finish("complete");
        else setTimeout(pump, 4);
      };
      pump();
    }, 40);
    return { device, stream, capture };
  }

  // ---- recorder views --------------------------------------------------

  private openView(recId: number, columns: number, followS: number | null, t0: number | null, t1: number | null): { view: number } {
    const rec = this.server.recording(recId);
    const view: View = { id: this.server.allocView(), rec, columns, followS, t0: t0 ?? 0, t1: t1 ?? 0, sequence: 0, timer: null };
    this.views.set(view.id, view);
    this.applyView(view);
    return { view: view.id };
  }

  private updateView(id: number, columns: number | null, followS: number | null, t0: number | null, t1: number | null): void {
    const view = this.views.get(id);
    if (!view) throw new DemoError("view_not_found", `no view ${id}`);
    if (columns !== null) view.columns = columns;
    view.followS = followS;
    if (followS === null) {
      view.t0 = t0 ?? view.t0;
      view.t1 = t1 ?? view.t1;
    }
    this.applyView(view);
  }

  private applyView(view: View): void {
    if (view.timer !== null) clearInterval(view.timer);
    view.timer = null;
    if (view.followS !== null) {
      view.timer = setInterval(() => this.sendViewFrame(view), 50);
    }
    setTimeout(() => this.sendViewFrame(view), 0);
  }

  private closeView(id: number): void {
    const view = this.views.get(id);
    if (view?.timer !== null && view?.timer !== undefined) clearInterval(view.timer);
    this.views.delete(id);
  }

  private sendViewFrame(view: View): void {
    if (this.closed || !this.views.has(view.id)) return;
    const { rec } = view;
    const head = this.server.recordingHead(rec);
    let first: number;
    let last: number;
    if (view.followS !== null) {
      last = head;
      first = Math.max(0, head - Math.round(view.followS * rec.rate));
    } else {
      first = Math.max(0, Math.round(view.t0 * rec.rate));
      last = Math.min(head, Math.round(view.t1 * rec.rate));
    }
    if (last <= first) return;
    const built = rec.analog
      ? buildAnalogView(rec.channels, rec.calibration, rec.rate, first, last, view.columns)
      : buildDigitalView(this.server.scene, rec.channels, rec.rate, first, last, view.columns);
    this.deliver(viewFrame({
      view: view.id,
      sequence: view.sequence++,
      kind: built.kind,
      channelCount: rec.channels.length,
      follow: view.followS !== null,
      itemCount: built.itemCount,
      firstSample: built.firstSample,
      samplesPerItem: built.samplesPerItem,
      payload: built.payload,
    }));
  }

  private async waitForPattern(recId: number, pattern: string, timeoutMs: number): Promise<unknown> {
    const server = this.server;
    const rec = server.recording(recId);
    const from = server.recordingHead(rec) / rec.rate;
    const [match] = server.searchMatches(rec, pattern, from, from + timeoutMs / 1000, 1);
    if (!match) return { timeout: true };
    const delay = Math.max(0, (match.t_s - from) * 1000);
    await new Promise((resolve) => setTimeout(resolve, delay));
    return { match };
  }

  // ---- terminals -------------------------------------------------------

  private writeTerm(session: Session, text: string): void {
    this.deliver(termFrame(session.id, session.sequence++, text));
  }

  private openTerminal(source: string, decode: { rec: number; decoder: DecoderId; channels: ChannelMapping[]; options: OptionSetting[] } | null) {
    const server = this.server;
    const info = server.terminalSources().find((s) => s.source === source);
    if (!info) throw new DemoError("source_not_found", `no terminal source ${source}`);
    const session: Session = { id: server.allocSession(), source, sequence: 0, timers: [], input: "" };
    this.sessions.set(session.id, session);
    const start = (fn: () => void, everyMs: number): void => {
      session.timers.push(setInterval(fn, everyMs));
    };
    const later = (text: string): void => {
      setTimeout(() => {
        if (this.sessions.has(session.id)) this.writeTerm(session, text);
      }, 20);
    };

    if (source === "serial:/dev/ttyACM0") {
      later(ESP_BOOT);
    } else if (source === "serial:/dev/ttyUSB0") {
      let i = 0;
      start(() => this.writeTerm(session, NMEA_LINES[i++ % NMEA_LINES.length]!), 1000);
    } else if (source === "log") {
      later(server.logHistory() || "Server log: actions by the UI and by agents appear here.\r\n");
    } else if (source === "scpi") {
      later("SCPI console. Try *IDN? or SYST:VERS?\r\n");
    } else if (source === "decode" && decode) {
      const rec = server.recording(decode.rec);
      const refusal = server.decode(decode.decoder, decode.channels, decode.options, rec, 0, 0, 0).note;
      if (refusal) {
        later(`${refusal}\r\n`);
      } else {
        let cursor = server.recordingHead(rec) / rec.rate;
        start(() => {
          const head = server.recordingHead(rec) / rec.rate;
          const items = server.decode(decode.decoder, decode.channels, decode.options, rec, cursor, head, 0).annotations;
          cursor = head;
          // A decode terminal scrolls fast enough at one line per second: more would be unreadable, not more real.
          const text = decode.decoder === "uart"
            ? items.slice(0, 24).map((a) => (a.kind.kind === "data" ? String.fromCharCode(a.kind.value) : "")).join("").replace(/\r?\n/g, "\r\n")
            : `${items.slice(0, 12).map((a) => a.text).join(" ")}\r\n`;
          if (text.trim()) this.writeTerm(session, text);
        }, 1000);
      }
    }
    return { session: session.id, source, kind: info.kind, label: info.label, state: "open", ...(info.kind === "serial" ? { serial: { baud: 115200, data_bits: 8, parity: "none", stop_bits: "one" } } : {}) };
  }

  private writeToTerminal(id: number, data: string): void {
    const session = this.sessions.get(id);
    if (!session) throw new DemoError("session_not_found", `no terminal session ${id}`);
    if (session.source === "log" || session.source === "decode") return;
    if (session.source === "scpi") {
      session.input += data;
      let at: number;
      while ((at = session.input.search(/[\r\n]/)) >= 0) {
        const line = session.input.slice(0, at).trim();
        session.input = session.input.slice(at + 1);
        if (line) this.writeTerm(session, `${scpiReply(line)}\r\n`);
      }
      return;
    }
    if (session.source === "serial:/dev/ttyUSB0") return;
    for (const char of data) {
      if (char === "\r" || char === "\n") {
        const line = session.input.trim();
        session.input = "";
        this.writeTerm(session, `\r\n${line ? `${espReply(line)}\r\n` : ""}esp> `);
      } else if (char === "\x7f" || char === "\b") {
        if (session.input) {
          session.input = session.input.slice(0, -1);
          this.writeTerm(session, "\b \b");
        }
      } else {
        session.input += char;
        this.writeTerm(session, char);
      }
    }
  }
}

function scpiReply(line: string): string {
  const upper = line.toUpperCase();
  if (upper === "*IDN?") return "AWIC,Simulated bench,DEMO,0";
  if (upper.startsWith("SYST:VERS")) return "1999.0";
  if (upper === "*OPC?") return "1";
  return `-113,"Undefined header; the demo answers *IDN?, *OPC? and SYST:VERS?"`;
}

function espReply(line: string): string {
  switch (line.toLowerCase()) {
    case "help":
      return "commands: help, status, temp, led on, led off";
    case "status":
      return "uptime 3421 s, heap 214 kB free, wifi off";
    case "temp":
      return "23.4 C (sensor 0x48)";
    case "led on":
      return "LED on";
    case "led off":
      return "LED off";
    default:
      return `unknown command: ${line}`;
  }
}

export const DECODERS = [
  {
    id: "uart",
    name: "UART",
    channels: ["rx"],
    options: [
      { type: "int", id: "baud", description: "Baud rate; 0 detects it from the line's pulse widths", default: 115200, min: 0 },
      { type: "int", id: "data_bits", description: "Data bits per frame", default: 8, min: 5, max: 9 },
      { type: "enum", id: "parity", description: "Parity checking", default: "none", values: ["none", "even", "odd"] },
    ],
  },
  { id: "i2c", name: "I2C", channels: ["scl", "sda"], options: [] },
  {
    id: "spi",
    name: "SPI",
    channels: ["clk", "mosi", "miso", "cs"],
    options: [{ type: "enum", id: "mode", description: "CPOL/CPHA mode", default: "mode0", values: ["mode0", "mode1", "mode2", "mode3"] }],
  },
] as const;

