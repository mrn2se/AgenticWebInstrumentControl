import { analogVolts } from "./scene";

/**
 * What the simulated agent does on the demo bench: a plausible debugging
 * session, one tool call at a time. Every value it reports matches the demo's
 * signals, so the MCP tab and the Graph tab tell the same story.
 */
export interface ScriptedCall {
  readonly tool: string;
  readonly summary: string;
  readonly args: unknown;
  readonly result: unknown;
  readonly image?: boolean;
  readonly error?: boolean;
  readonly durationMs: number;
  readonly log: string;
}

const CAPTURE = "capture://7f3c1a52-demo";
const LOGIC = "3-3.4.1";

export const MCP_SCRIPT: readonly ScriptedCall[] = [
  {
    tool: "list_instruments",
    summary: "2 instruments",
    args: {},
    result: { instruments: [{ device: LOGIC, kind: "digital", channels: 8 }, { device: "3-3.4.2", kind: "analog", channels: 2 }] },
    durationMs: 3,
    log: "list_instruments",
  },
  {
    tool: "configure_acquisition",
    summary: `${LOGIC}: 8 channels at 8 MHz, 160000 samples`,
    args: { device: LOGIC, rate_hz: 8000000, channels: [0, 1, 2, 3, 4, 5, 6, 7], samples: 160000 },
    result: { rate_hz: 8000000, samples: 160000 },
    durationMs: 12,
    log: `acq.configure device=${LOGIC} rate_hz=8000000`,
  },
  {
    tool: "capture",
    summary: `${CAPTURE}, 20 ms`,
    args: { device: LOGIC },
    result: { capture: CAPTURE, duration_s: 0.02, channels: { D0: { edges: 142 }, D1: { edges: 148 }, D7: { edges: 40, frequency_hz: 1000 } } },
    durationMs: 41,
    log: `acq.start device=${LOGIC}`,
  },
  {
    tool: "survey_channels",
    summary: "UART on D0, I2C on D1/D2, SPI on D3-D6, clock on D7",
    args: { capture: CAPTURE },
    result: {
      guesses: [
        { protocol: "uart", channel: 0, baud: 115200, fit: 0.99 },
        { protocol: "i2c", scl: 1, sda: 2, clock_hz: 400000 },
        { protocol: "spi", clk: 4, data: [5, 6], cs: 3, mode: 0, clock_hz: 1000000 },
        { protocol: "clock", channel: 7, frequency_hz: 1000, duty: 0.25 },
      ],
    },
    durationMs: 18,
    log: "survey",
  },
  {
    tool: "decode",
    summary: "uart: temp=23.4C rpm=1480 ok",
    args: { capture: CAPTURE, decoder: "uart", channels: [{ role: "rx", channel: 0 }], format: "transactions" },
    result: { transactions: [{ t_s: 0.0002, text: "temp=23.4C rpm=1480 ok\r\n", count: 24 }], summary: { transactions: 1, nacks: 0, errors: 0 } },
    durationMs: 9,
    log: "capture.decode decoder=uart",
  },
  {
    tool: "decode",
    summary: "i2c: 0x48 read 17 60, 0x3C write 00 AF",
    args: { capture: CAPTURE, decoder: "i2c", channels: [{ role: "scl", channel: 1 }, { role: "sda", channel: 2 }], format: "transactions" },
    result: {
      transactions: [
        { t_s: 0.0025, address: 72, read: true, data: "17 60", count: 2, repeated_start: true },
        { t_s: 0.0027, address: 60, read: false, data: "00 AF", count: 2 },
      ],
      summary: { transactions: 2, nacks: 0, errors: 0 },
    },
    durationMs: 7,
    log: "capture.decode decoder=i2c",
  },
  {
    tool: "render_view",
    summary: "waveform of CH1 and CH2, 1 picture",
    args: { capture: "capture://b92e04d1-demo", kind: "waveform", channels: [0, 1] },
    result: { kind: "waveform", metrics: [{ name: "CH1 Vpp", text: "2.0 V" }, { name: "CH2 frequency", text: "10 kHz" }] },
    image: true,
    durationMs: 64,
    log: "capture.render kind=waveform",
  },
  {
    tool: "measure",
    summary: "D7: 1 kHz, 25 % duty",
    args: { capture: CAPTURE, channel: 7, measurements: ["frequency", "duty"] },
    result: { results: [{ kind: "frequency", value: 1000, unit: "Hz" }, { kind: "duty", value: 25, unit: "%" }] },
    durationMs: 4,
    log: "capture.measure channel=7",
  },
  {
    tool: "serial_write",
    summary: "ESP32-C6: status",
    args: { source: "serial:/dev/ttyACM0", data: "status\n" },
    result: { written: 7 },
    durationMs: 2,
    log: "term.write source=serial:/dev/ttyACM0",
  },
  {
    tool: "power_cycle_usb_port",
    summary: "refused: port feeds hub 3-3.4",
    args: { hub: "3-3", port: 4 },
    result: { error: "port_protected", message: "feeds hub 3-3.4: switching it cuts every device behind it" },
    error: true,
    durationMs: 1,
    log: "usb.cycle hub=3-3 port=4 refused: port_protected",
  },
];

/** The picture a `render_view` entry shows: the two scope channels, drawn from the same signal the Graph tab gets. */
export function waveformSvg(): string {
  const width = 640;
  const height = 240;
  const line = (channel: number, color: string): string => {
    const points: string[] = [];
    for (let x = 0; x <= width; x += 2) {
      const volts = analogVolts(channel, (x / width) * 0.002);
      points.push(`${x},${(height / 2 - volts * 40).toFixed(1)}`);
    }
    return `<polyline fill="none" stroke="${color}" stroke-width="1.5" points="${points.join(" ")}"/>`;
  };
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}"><rect width="${width}" height="${height}" fill="#16171a"/><line x1="0" x2="${width}" y1="${height / 2}" y2="${height / 2}" stroke="#34363b"/>${line(0, "#6ea8fe")}${line(1, "#4ada6f")}<text x="10" y="20" fill="#e7e8ea" font-family="sans-serif" font-size="13">Simulated: CH1 calibration square, CH2 sine, 2 ms</text></svg>`;
}
