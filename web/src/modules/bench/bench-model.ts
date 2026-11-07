import type { SimStatus } from "../../proto/types";

/** One row of the test-point table: what it is, and what is clipped to it. */
export interface TestPointRow {
  readonly id: string;
  readonly name: string;
  readonly description: string;
  readonly kind: string;
  /** `"Logic #1 D3"` for every probe known to be on it. */
  readonly probes: readonly string[];
}

/** A short name for an instrument, from its label. */
export function instrumentShortName(label: string): string {
  if (/#2/.test(label)) return "Logic #2";
  if (/logic/i.test(label)) return "Logic #1";
  if (/scope|oscillo/i.test(label)) return "Scope";
  return label;
}

/** The channel name an instrument's UI uses: D0.. for logic, CH1.. for a scope. */
export function channelName(label: string, channel: number): string {
  return /scope|oscillo/i.test(label) ? `CH${channel + 1}` : `D${channel}`;
}

/**
 * The board's test points with the probes on each. Where the shuffled
 * analyzer's channels are is a secret, but not which points it is clipped
 * to: those show as `"Logic #1 D?"`. `hiddenProbes` counts the probes whose
 * channel is to be found.
 */
export function testPointRows(status: SimStatus): { rows: TestPointRow[]; hiddenProbes: number } {
  const probes = new Map<string, string[]>();
  let hiddenProbes = 0;
  for (const inst of status.instruments) {
    const short = instrumentShortName(inst.label);
    const secret = inst.probes.filter((p) => !p.test_point).length;
    hiddenProbes += secret;
    if (secret > 0) {
      const unknown = /scope|oscillo/i.test(inst.label) ? "CH?" : "D?";
      for (const point of inst.probed_points) {
        const list = probes.get(point) ?? [];
        list.push(`${short} ${unknown}`);
        probes.set(point, list);
      }
    }
    for (const probe of inst.probes) {
      if (!probe.test_point) continue;
      const list = probes.get(probe.test_point) ?? [];
      list.push(`${short} ${channelName(inst.label, probe.channel)}`);
      probes.set(probe.test_point, list);
    }
  }
  const rows = status.board.test_points.map((tp) => ({
    id: tp.id,
    name: tp.name,
    description: tp.description,
    kind: tp.kind,
    probes: probes.get(tp.id) ?? [],
  }));
  return { rows, hiddenProbes };
}

/** Whole seconds as `1 h 02 min`, `4 min 05 s` or `12.3 s`. */
export function formatUptime(seconds: number | null | undefined): string {
  if (seconds === null || seconds === undefined) return "not running";
  if (seconds < 60) return `${seconds.toFixed(1)} s`;
  const s = Math.floor(seconds);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  if (h > 0) return `${h} h ${String(m).padStart(2, "0")} min`;
  return `${m} min ${String(sec).padStart(2, "0")} s`;
}

/** The pot's position as a slider value, on a log scale (470 Ω .. 100 kΩ). */
export const R2_MIN = 470;
export const R2_MAX = 100_000;

export function r2ToSlider(ohm: number): number {
  const clamped = Math.min(R2_MAX, Math.max(R2_MIN, ohm));
  return Math.round((1000 * Math.log(clamped / R2_MIN)) / Math.log(R2_MAX / R2_MIN));
}

export function sliderToR2(value: number): number {
  const ohm = R2_MIN * Math.pow(R2_MAX / R2_MIN, value / 1000);
  // Two significant figures, like a pot's scale.
  const digits = Math.pow(10, Math.floor(Math.log10(ohm)) - 1);
  return Math.round(ohm / digits) * digits;
}

export function formatOhm(ohm: number): string {
  return ohm >= 1000 ? `${(ohm / 1000).toFixed(ohm >= 10_000 ? 0 : 1)} kΩ` : `${Math.round(ohm)} Ω`;
}

export function formatHz(hz: number): string {
  return hz >= 1000 ? `${(hz / 1000).toFixed(2)} kHz` : `${hz.toFixed(0)} Hz`;
}

/** A ready-made request to paste into an AI agent connected to AWIC over MCP. */
export interface AgentPrompt {
  readonly id: string;
  readonly title: string;
  /** What to do on this page before or while the agent works, if anything. */
  readonly yourPart: string;
  readonly text: string;
}

/**
 * What to tell an agent to try the bench with. An agent sees only what it
 * would see on real hardware — the same MCP tools, no view of this page —
 * so each prompt carries the context a person at the bench would give: what
 * the board is, which device is which. Never the answers: the wiring of the
 * shuffled analyzer, the fault in force and U3's part number stay for the
 * agent to find.
 */
export function agentPrompts(status: SimStatus): AgentPrompt[] {
  const byPort = (port: number): string =>
    status.instruments.find((i) => i.port === port)?.device ?? "?";
  const logic = byPort(2);
  const scope = byPort(3);
  const logic2 = byPort(4);
  const board = status.board;
  const intro =
    `AWIC (over MCP) is connected to a bench with a small board under test: an MCU with a serial console, ` +
    `a temperature sensor on I2C, a SPI flash, a heartbeat LED and a 555 timer. ` +
    `The board is powered from USB hub ${status.hub} port ${board.port}; its console is the serial source ` +
    `${board.serial_source} (${board.console}).`;
  return [
    {
      id: "wiring",
      title: "Find out how the logic analyzer is wired",
      yourPart: "Nothing — just watch the MCP tab.",
      text:
        `${intro} The logic analyzer ${logic} has its eight probes on the board's test points TP1-TP8: ` +
        `UART TX, UART RX, I2C SCL, I2C SDA, SPI SCK, SPI MOSI, SPI CS# and the LED — but nobody wrote ` +
        `down which channel went where. Work out which channel D0-D7 is on which test point, using only ` +
        `the instruments, and tell me how you know for each one.`,
    },
    {
      id: "console",
      title: "Talk to the firmware",
      yourPart: "Nothing.",
      text:
        `${intro} Find out what the firmware can do, read the temperature, and then show me what one ` +
        `sensor read looks like on the wire: which analyzer channels carry it (the analyzer ${logic} is on ` +
        `the MCU's signals in an unknown order), the address, the bytes, and how long the transaction takes.`,
    },
    {
      id: "reboot",
      title: "Catch a reboot",
      yourPart: "Nothing.",
      text:
        `${intro} Record the board's signals on ${logic} while you power-cycle it, then tell me what ` +
        `happens in the first second after power-on: in what order the console, the sensor bus, the ` +
        `flash bus and the LED start, with times. Keep the console open across the power cycle.`,
    },
    {
      id: "u3",
      title: "What is U3?",
      yourPart:
        "Press U3's keys and switches above when the agent asks, then check its answer with the part-number box.",
      text:
        `On the board under test there is also U3, a 16-pin CMOS DIP with its markings sanded off, on 5 V ` +
        `(pin 16) and ground (pin 8), driving a seven-segment display. Push buttons KEY1 and KEY2 pull pins ` +
        `1 and 15 high while pressed; switches SW1 and SW2 set pins 2 and 3. The logic analyzer ${logic2} ` +
        `is clipped onto U3's pins 5, 6, 7, 9, 10, 11, 12 and 13, on D0-D7 in that order. You cannot press ` +
        `the keys yourself: start a recording on ${logic2}, tell me what to press or switch, one step at a ` +
        `time, and wait for me to say "done". From the pin behaviour and the pinout, tell me which part U3 ` +
        `is, and why.`,
    },
    {
      id: "fault",
      title: "Hunt a fault",
      yourPart:
        "Click \"Start a mystery\" under Faults first (or tick a fault you want to test the agent on). Don't reveal it until the agent has answered.",
      text:
        `${intro} Since this morning something on the board misbehaves now and then, and I don't know ` +
        `what. Available: the logic analyzer ${logic} on the MCU's signals TP1-TP8 (UART TX/RX, I2C ` +
        `SCL/SDA, SPI SCK/MOSI/CS#, LED, in an unknown order), the oscilloscope ${scope} with CH1 on the ` +
        `555's timing capacitor and CH2 on its output, and the console. Find the problem — it may take a ` +
        `minute or more to show — and give me the evidence: what you measured, where and when.`,
    },
    {
      id: "pot",
      title: "Measure the 555 as I turn its pot",
      yourPart: "Move the R2 slider under \"555 timer\" when the agent asks.",
      text:
        `The oscilloscope ${scope} has CH1 on a 555 astable's timing capacitor and CH2 on its output; ` +
        `R1 = 1 kΩ and C = 100 nF, and R2 is a pot. Measure the frequency and duty cycle now and work out ` +
        `R2. Then ask me to turn the pot, wait for me to say "done", measure again and give me the new R2. ` +
        `Check that the capacitor swings between a third and two thirds of the 5 V supply.`,
    },
  ];
}
