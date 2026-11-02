import type { AnnotationKindWire, DecoderId } from "../proto/types";

/**
 * The demo bench's wires, as a pure function of time: what a logic analyzer
 * or an oscilloscope would see at any instant. Nothing stores samples, so a
 * capture, a recording of any length and a zoom into either are all just
 * different questions asked of the same signal.
 *
 * Every line repeats with {@link PERIOD_S}. Each burst records what it put on
 * the wire as it builds the edges, so a "decode" of the demo reads back
 * exactly the bytes that are drawn. Checked once against the real decoders in
 * `awic-decode`: fed these wires, they report the same items in the same order.
 */
export const PERIOD_S = 0.02;

export const UART_BAUD = 115200;
export const I2C_HZ = 400_000;
export const SPI_HZ = 1_000_000;

/** Which channel carries what. Decoding any other mapping finds nothing, as on a real bench. */
export const WIRING = {
  uart: { rx: 0 },
  i2c: { scl: 1, sda: 2 },
  spi: { cs: 3, clk: 4, mosi: 5, miso: 6 },
  clock: 7,
} as const;

export const CHANNEL_NAMES = ["UART TX", "I2C SCL", "I2C SDA", "SPI CS", "SPI SCK", "SPI MOSI", "SPI MISO", "1 kHz clock"] as const;

export interface SceneAnnotation {
  readonly decoder: DecoderId;
  readonly t0: number;
  readonly t1: number;
  readonly level: number;
  readonly kind: AnnotationKindWire;
  readonly text: string;
}

const UART_LINE = "temp=23.4C rpm=1480 ok\r\n";

class WireBuilder {
  readonly edges: number[][] = Array.from({ length: 8 }, () => []);
  readonly levels: number[];
  readonly initial: readonly number[];
  readonly annotations: SceneAnnotation[] = [];

  constructor(initial: readonly number[]) {
    this.initial = initial;
    this.levels = [...initial];
  }

  set(channel: number, t: number, level: number): void {
    if (this.levels[channel] === level) return;
    this.levels[channel] = level;
    this.edges[channel]!.push(t);
  }

  note(decoder: DecoderId, t0: number, t1: number, level: number, kind: AnnotationKindWire, text: string): void {
    this.annotations.push({ decoder, t0, t1, level, kind, text });
  }
}

function hex2(value: number): string {
  return value.toString(16).toUpperCase().padStart(2, "0");
}

function addUart(w: WireBuilder, start: number): number {
  const bit = 1 / UART_BAUD;
  const ch = WIRING.uart.rx;
  let t = start;
  for (const char of UART_LINE) {
    const byte = char.charCodeAt(0);
    w.set(ch, t, 0);
    for (let i = 0; i < 8; i++) w.set(ch, t + bit * (i + 1), (byte >> i) & 1);
    w.set(ch, t + bit * 9, 1);
    w.note("uart", t, t + bit * 10, 0, { kind: "data", value: byte }, `0x${hex2(byte)} '${char}'`);
    t += bit * 10;
  }
  return t;
}

/** One I2C byte plus its ninth clock: the slot `ack` says who pulled SDA low. */
function i2cByte(w: WireBuilder, start: number, byte: number, ack: boolean): number {
  const period = 1 / I2C_HZ;
  const { scl, sda } = WIRING.i2c;
  let t = start;
  for (let i = 7; i >= -1; i--) {
    const bit = i >= 0 ? (byte >> i) & 1 : ack ? 0 : 1;
    w.set(scl, t, 0);
    w.set(sda, t + period / 4, bit);
    w.set(scl, t + period / 2, 1);
    t += period;
  }
  w.set(scl, t, 0);
  return t;
}

function addI2c(w: WireBuilder, start: number): number {
  const period = 1 / I2C_HZ;
  const { scl, sda } = WIRING.i2c;
  const startCondition = (t: number, repeated: boolean): number => {
    if (repeated) {
      w.set(sda, t, 1);
      w.set(scl, t + period / 4, 1);
    }
    w.set(sda, t + period / 2, 0);
    w.note("i2c", t, t + period, 0, repeated ? { kind: "repeated_start" } : { kind: "start" }, repeated ? "Sr" : "S");
    return t + period;
  };
  const address = (t: number, addr: number, read: boolean): number => {
    const next = i2cByte(w, t, (addr << 1) | (read ? 1 : 0), true);
    w.note("i2c", t, next - period, 1, { kind: "address", address: addr, read }, `0x${hex2(addr)} ${read ? "R" : "W"}`);
    w.note("i2c", next - period, next, 2, { kind: "ack" }, "A");
    return next;
  };
  const data = (t: number, value: number, ack: boolean): number => {
    const next = i2cByte(w, t, value, ack);
    w.note("i2c", t, next - period, 1, { kind: "data", value }, `0x${hex2(value)}`);
    w.note("i2c", next - period, next, 2, ack ? { kind: "ack" } : { kind: "nack" }, ack ? "A" : "N");
    return next;
  };
  const stopCondition = (t: number): number => {
    w.set(sda, t, 0);
    w.set(scl, t + period / 4, 1);
    w.set(sda, t + period / 2, 1);
    w.note("i2c", t, t + period, 0, { kind: "stop" }, "P");
    return t + period;
  };

  // A temperature sensor at 0x48: point at register 0, read two bytes back.
  let t = startCondition(start, false);
  t = address(t, 0x48, false);
  t = data(t, 0x00, true);
  t = startCondition(t, true);
  t = address(t, 0x48, true);
  t = data(t, 0x17, true);
  t = data(t, 0x60, false);
  t = stopCondition(t);
  // A display at 0x3C: two command bytes.
  t = startCondition(t + period * 8, false);
  t = address(t, 0x3c, false);
  t = data(t, 0x00, true);
  t = data(t, 0xaf, true);
  return stopCondition(t);
}

function addSpi(w: WireBuilder, start: number): number {
  const period = 1 / SPI_HZ;
  const { cs, clk, mosi, miso } = WIRING.spi;
  // A flash chip answering "read JEDEC ID" (mode 0, MSB first).
  const words: ReadonlyArray<readonly [number, number]> = [
    [0x9f, 0xff],
    [0x00, 0xef],
    [0x00, 0x40],
    [0x00, 0x18],
  ];
  w.set(cs, start, 0);
  w.note("spi", start, start + period, 0, { kind: "chip_select", active: true }, "CS active");
  let t = start + period;
  for (const [out, back] of words) {
    const wordStart = t;
    for (let i = 7; i >= 0; i--) {
      w.set(mosi, t, (out >> i) & 1);
      w.set(miso, t, (back >> i) & 1);
      w.set(clk, t + period / 2, 1);
      w.set(clk, t + period, 0);
      t += period;
    }
    w.note("spi", wordStart, t, 1, { kind: "word", mosi: out, miso: back }, `MOSI=0x${out.toString(16).toUpperCase()} MISO=0x${back.toString(16).toUpperCase()}`);
    t += period;
  }
  w.set(mosi, t, 0);
  w.set(miso, t, 1);
  w.set(cs, t, 1);
  w.note("spi", t, t + period, 0, { kind: "chip_select", active: false }, "CS idle");
  return t + period;
}

function addClock(w: WireBuilder): void {
  const cycles = Math.round(PERIOD_S * 1000);
  for (let i = 0; i < cycles; i++) {
    w.set(WIRING.clock, i / 1000, 1);
    w.set(WIRING.clock, i / 1000 + 0.00025, 0);
  }
}

export class LogicScene {
  private readonly edges: readonly (readonly number[])[];
  private readonly initial: readonly number[];
  private readonly annotations: readonly SceneAnnotation[];

  constructor() {
    const w = new WireBuilder([1, 1, 1, 1, 0, 0, 1, 0]);
    const afterUart = addUart(w, 0.0002);
    const afterI2c = addI2c(w, afterUart + 0.0002);
    addSpi(w, afterI2c + 0.0002);
    addClock(w);
    this.edges = w.edges;
    this.initial = w.initial;
    this.annotations = [...w.annotations].sort((a, b) => a.t0 - b.t0);
  }

  levelAt(channel: number, t: number): number {
    const edges = this.edges[channel];
    if (!edges) return 0;
    const phase = t - Math.floor(t / PERIOD_S) * PERIOD_S;
    return (this.initial[channel]! + upperCount(edges, phase)) % 2;
  }

  /**
   * Every edge of `channel` in `(t0, t1)`, in order. An edge exactly at `t0` is
   * already part of `levelAt(channel, t0)`, so a walk from there must not count it twice.
   */
  *edgesBetween(channel: number, t0: number, t1: number): Generator<{ t: number; rising: boolean }> {
    const edges = this.edges[channel];
    if (!edges || edges.length === 0) return;
    for (let loop = Math.floor(t0 / PERIOD_S); loop * PERIOD_S < t1; loop++) {
      const base = loop * PERIOD_S;
      for (let i = upperCount(edges, t0 - base); i < edges.length; i++) {
        const t = base + edges[i]!;
        if (t >= t1) return;
        yield { t, rising: (this.initial[channel]! + i + 1) % 2 === 1 };
      }
    }
  }

  countEdges(channel: number, t0: number, t1: number): number {
    const edges = this.edges[channel];
    if (!edges) return 0;
    let count = 0;
    for (const _ of this.edgesBetween(channel, t0, t1)) count++;
    return count;
  }

  /** Writes `n` packed samples (bit n = Dn, only the masked channels) from `t0` at `rate` into `out`. */
  fillPacked(out: Uint8Array, n: number, t0: number, rate: number, channels: readonly number[]): void {
    out.fill(0, 0, n);
    const t1 = t0 + n / rate;
    for (const channel of channels) {
      const bit = 1 << channel;
      let level = this.levelAt(channel, t0);
      let pos = 0;
      for (const edge of this.edgesBetween(channel, t0, t1)) {
        const idx = Math.min(n, sampleAtOrAfter((edge.t - t0) * rate));
        if (level) for (let i = pos; i < idx; i++) out[i]! |= bit;
        pos = Math.max(pos, idx);
        level ^= 1;
      }
      if (level) for (let i = pos; i < n; i++) out[i]! |= bit;
    }
  }

  /** When the first item of a decoder's traffic starts, within one period. */
  firstAt(decoder: DecoderId): number {
    return this.annotations.find((a) => a.decoder === decoder)?.t0 ?? 0;
  }

  /** The decoded traffic in `[t0, t1)`, whole items only. */
  *annotationsBetween(decoder: DecoderId, t0: number, t1: number): Generator<SceneAnnotation> {
    for (let loop = Math.floor(t0 / PERIOD_S); loop * PERIOD_S < t1; loop++) {
      const base = loop * PERIOD_S;
      for (const a of this.annotations) {
        if (a.decoder !== decoder) continue;
        const start = base + a.t0;
        const end = base + a.t1;
        if (start >= t0 && end <= t1) yield { ...a, t0: start, t1: end };
        else if (start >= t1) break;
      }
    }
  }
}

/** The first sample at or after a position in samples, not moved a whole sample by a double's rounding. */
export function sampleAtOrAfter(position: number): number {
  return Math.ceil(position - 1e-6);
}

/** How many of sorted `values` are at or before `x`. */
function upperCount(values: readonly number[], x: number): number {
  let lo = 0;
  let hi = values.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (values[mid]! <= x) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}

const CAL_HZ = 1000;
const SINE_HZ = 10_000;

/** How long each oscilloscope channel takes to repeat. */
export function analogPeriodS(channel: number): number {
  return channel === 0 ? 1 / CAL_HZ : 1 / SINE_HZ;
}

/** Volts on an oscilloscope channel without noise: CH1 a probe-calibration square, CH2 a slightly distorted sine. */
export function analogVolts(channel: number, t: number): number {
  if (channel === 0) {
    const phase = t * CAL_HZ - Math.floor(t * CAL_HZ);
    const high = phase < 0.5;
    const since = (high ? phase : phase - 0.5) / CAL_HZ;
    const settle = Math.exp(-since / 4e-6);
    return high ? 2 * (1 - settle) + 0.08 * Math.exp(-since / 12e-6) * Math.sin(since * 2 * Math.PI * 180e3) : 2 * settle;
  }
  const w = 2 * Math.PI * SINE_HZ * t;
  return 1.5 * Math.sin(w) + 0.12 * Math.sin(3 * w);
}

/** A repeatable ±1 code of noise per sample, so re-reading a sample gives the same value. */
export function noiseCode(sample: number, channel: number): number {
  let h = Math.imul((sample ^ (channel * 0x9e3779b1)) >>> 0, 0x85ebca6b);
  h ^= h >>> 13;
  h = Math.imul(h, 0xc2b2ae35);
  return ((h >>> 28) % 3) - 1;
}
