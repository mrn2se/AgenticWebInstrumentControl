# Demos: try AWIC without hardware

Three ways to see what AWIC does before you plug anything in, from a click in the browser to an AI
agent debugging a simulated board on your own machine.

| | [Web demo](#1-the-web-demo) | [Virtual bench](#2-the-virtual-bench) | [Example reports](#3-example-reports) |
|---|---|---|---|
| Needs | A browser | The `awic` executable (Linux) | Nothing |
| What runs | The real web UI against a bench simulated in the page | The real server with a simulated board and instruments | Nothing: reports from a real bench |
| Instruments | Recorded-looking signals that repeat every 20 ms | A board with firmware that reacts to you, in real time | FX2 logic analyzer and oscilloscope |
| AI agent | A scripted agent session to watch | Your own agent, over MCP | Read what an agent found |
| Good for | A first look in a minute | Trying everything, demos, puzzles, agent tests | Seeing results on real hardware |

## 1. The web demo

**[Open the web demo](https://mrn2se.github.io/AgenticWebInstrumentControl/)**: no install, nothing
leaves your browser.

It is AWIC's real web UI, built for GitHub Pages, with the server replaced by a simulation inside
the page:

- **Graph**: a logic analyzer and an oscilloscope on a bench with UART, I²C and SPI traffic and a
  1 kHz clock. Capture, zoom, measure, decode, search and record as on hardware; the decoders read
  back exactly what is drawn.
- **USB**: a hub tree with switchable ports, protection and labels.
- **Terminal**: two serial ports that come and go with their USB ports, the SCPI console and the
  server log.
- **MCP**: a scripted agent session plays out, one tool call at a time, with the pictures it got
  back, so you can see what an agent does with AWIC.

The welcome dialog on the same page writes the install commands for your setup.

The bench in the page is fixed and short: everything repeats every 20 ms and nothing answers when
you type. For a board that behaves, use the virtual bench.

## 2. The virtual bench

```sh
./awic-linux-x86_64 --sim
```

Then open <http://127.0.0.1:8080/>. `--sim` (or `[sim] enabled = true` in the config) starts the
real server with a simulated board under test on a virtual USB hub, next to any real instruments.
Everything virtual sits on bus 254 with vendor ID `ffff`, so it can never be mistaken for hardware.
Add `--set backends.native=false` on a machine with no instruments at all.

### What is on the bench

**The AWIC trainer board.** Only its test points are reachable, as on a real board:

| Point | Signal |
|---|---|
| TP1, TP2 | The MCU's console, UART TX and RX, 115200 8N1 |
| TP3, TP4 | I²C SCL and SDA; a TMP102 temperature sensor polled every 500 ms |
| TP5–TP7 | SPI SCK, MOSI and CS# to a W25Q32 flash |
| TP8 | The heartbeat LED |
| TP9, TP10 | A 555 timer's output and timing capacitor, about 1 kHz, with a pot (R2) |
| U3 pins 5–13 | An unmarked chip's outputs (see the puzzle below) |

The MCU's firmware is simulated event by event. Its console prints a boot banner with the reset
cause and a temperature line every 2 s, and answers `help`, `version`, `uptime`, `temp`,
`led on|off|blink`, `log on|off`, `i2c scan`, `flash id`, `echo`, `reboot` and `crash`.

**The instruments**, each on its own port of the virtual hub:

| Port | Device | Clipped to |
|---|---|---|
| 1 | The board, with its USB-UART (a `serial` terminal source) | — |
| 2 | Logic analyzer #1, 8 channels | TP1–TP8, **in a random order** |
| 3 | Oscilloscope, 2 channels | CH1 on TP10, CH2 on TP9 |
| 4 | Logic analyzer #2, 8 channels | U3 pins 5, 6, 7, 9, 10, 11, 12, 13 on D0–D7 |

They are ordinary AWIC instruments: Single and Live capture, the recorder, decoders, the channel
survey, measurements, rendered views, SCPI and MCP all work on them. They sample the board's
waveforms in real time, so a command typed on the console shows up on the RX line, and switching a
hub port off makes its device drop off the bus, as hardware does. Power-cycling port 1 reboots the
board, and an open console waits and then shows the new boot banner.

### Things to try

- **Which channel is which?** Logic analyzer #1 is wired in a random order at every start. Find
  the console, the sensor bus, the flash and the LED, with the channel survey (Graph tab, or
  `survey_channels` over MCP) and the decoders.
- **What is U3?** A 16-pin CMOS chip with its markings sanded off, driving a seven-segment display.
  KEY1 and KEY2 are push buttons on pins 1 and 15; SW1 and SW2 are switches on pins 2 and 3. Press,
  switch, record its outputs, work out from the pinout and behaviour which part it is, and check
  your answer on the Bench tab.
- **Hunt a fault.** Each fault is injected into the waveforms and felt by the firmware, so it has
  to be found from the outside:

  | Fault | Symptom |
  |---|---|
  | `clock_glitch` | The temperature reading jumps to nonsense now and then |
  | `i2c_nack` | The console logs failed sensor reads |
  | `baud_drift` | The console garbles for a while, then recovers |
  | `spi_cs_glitch` | Flash checksum errors at random |
  | `watchdog_reset` | The board reboots by itself every half minute or so |
  | `runt_pulse` | Pulses on the 555's output that never reach full height |
  | `osc_jitter` | The 555's period varies far more than it should |
  | `key_bounce` | KEY1 sometimes counts twice |

  Pick faults on the Bench tab, or start a **mystery**: one at random, kept secret until you reveal
  it. `--set 'sim.faults=["mystery"]'` starts the server with one.
- **Turn the pot** on the Bench tab and measure the 555's new frequency on the scope.

The **Bench** tab shows the board, its test points and what is clipped to each, U3's keys and
display, the pot and the faults. It appears only when the virtual bench runs.

`--set sim.seed=N` repeats a run: the same wiring and the same fault timing. The seed in use is
shown on the Bench tab. `--set sim.shuffle=false` wires analyzer #1 in order.

### With an AI agent

The virtual bench is a safe place to let an agent loose: nothing it switches can break anything.
Next to AWIC's usual tools it offers four more:

| Tool | Does |
|---|---|
| `virtual_bench` | Describes the board, the test points, the instruments and the faults |
| `virtual_bench_set` | Sets faults, starts a mystery, turns the pot, reveals the secrets |
| `virtual_bench_input` | Presses U3's keys and sets its switches |
| `virtual_bench_guess` | Checks a guess at U3's part number |

With Claude Code, from an **empty folder**:

```sh
mkdir -p ~/awic-sim-try && cd ~/awic-sim-try
claude mcp add --transport http awic-sim http://127.0.0.1:8080/mcp
claude
```

Use an empty folder rather than AWIC's source tree: the source names the answers. Then ask, for
example:

- "Use the awic-sim tools. Find out which logic analyzer #1 channel is on which test point."
- "U3 on the virtual bench is an unmarked chip. Work out what part it is, then check with
  `virtual_bench_guess`."
- "Start a mystery fault on the virtual bench and find it with the instruments. Don't reveal it."

Every tool call shows up in the web UI's MCP tab, so you can watch the agent work.

## 3. Example reports

[example-report/](../example-report/README.md) holds what an agent and a person learned from AWIC on
a real bench: an I²C display, a bit-banged UART, CAN between two nodes, a sine and the scope's
calibration output, with the pictures AWIC drew and screenshots of the web UI.
