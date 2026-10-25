# AWIC — Features

What AWIC does today, what it does only in part, and what is planned. How each feature works is in
[SPECIFICATION.md](SPECIFICATION.md). Bug (B-nn) and feature (F-nn) numbers refer to findings from
bench runs; open work is tracked in [GitHub issues](https://github.com/mrn2se/AgenticWebInstrumentControl/issues).

The milestone tags (M0–M17) are the names the specification and the code use for each area.

## Overview

AWIC turns cheap USB bench instruments into three coordinated control surfaces over one shared
device core:

1. a **browser instrument panel**: live scope traces, logic captures, protocol decoding, a terminal;
2. an **MCP server**, so an AI agent drives the instruments as a first-class client;
3. an **SCPI socket**, so scripts and VISA tooling drive them like a bench instrument.

The instrument core owns every device handle and all device state; web, MCP and SCPI are thin
transports over it. That is what makes "an agent can do everything the UI can" true by construction.

**Not goals:** a replacement for PulseView as a desktop app; more than one host; calibration-grade
measurement (these are hobby instruments, and results say how far to trust them).

| Area | State |
|---|---|
| [Instruments](#instruments) | logic analyzer **available**; oscilloscope **available**, absolute volts uncalibrated |
| [USB tree and port power](#usb-tree-and-port-power-m1) | **available** |
| [Captures and measurements](#captures-and-measurements-m3-m4) | **available** |
| [Live recorder](#live-recorder-m12) | **available**; analog recordings end on a stall |
| [Pattern search and wait](#pattern-search-and-wait-m13) | **available** |
| [Protocol decoders](#protocol-decoders-and-channel-survey-m5) | **available**: UART, I²C, SPI, CAN, 1-Wire, survey |
| [Rendered views](#rendered-views-m16) | **available**: waveform, eye, histogram, anomalies |
| [Terminal](#terminal-m14) | **available**: serial, SCPI console, log, decoder output |
| [MCP server and monitor](#mcp-server-m7-and-monitor-m17) | **available**, 27 tools |
| [SCPI server](#scpi-server-m8) | **available**, VISA-conformant |
| [Web UI](#web-ui-m2-m15) | **available** |
| [Security](#security-m10) | **available**: optional token, origin guard |
| [Distribution](#distribution) | static binary, container image |
| [PSU](#psu-m6) | **planned** |
| [Export](#export-m9) | **planned** |

---

## Instruments

| Device | ID | What works |
|---|---|---|
| 8-channel FX2 logic analyzer (fx2lafw firmware) | `0925:3881` | D0–D7; rates up to 16 MS/s sustained (24 MHz is not offered, B-63) |
| 2-channel FX2 USB oscilloscope | `04b4:602a` → `1d50:608e` | CH1/CH2; 48/30/24/16/8/4/1 MHz and 500/200/100 kHz; ranges ±5/±2.5/±1.25/±0.5 V |

- Native Rust drivers over `nusb`: no libsigrok, no C library in the process. AWIC uploads the
  open sigrok fx2lafw firmware itself, on every enumeration, so a power cycle never leaves a dead
  device.
- Devices are identified by **USB bus path**, never VID:PID: an oscilloscope switched to logic
  mode has the same ID as the logic analyzer, and AWIC tells them apart and labels them by place.
- Instruments follow the bus across re-enumeration and power cycles.
- The logic analyzer resets itself after a stall and goes on streaming (B-20, B-25, B-26).
- A software trigger with pre-trigger samples and a "never fired" timeout (B-32); captures say
  whether and where they triggered.
- **Partial:** the oscilloscope's absolute amplitude is not calibrated: each range's true zero
  code is not yet recorded (B-14, SPEC §2.5.1), so absolute volts carry an "offset not calibrated"
  note. Frequency and timing are verified (999.998–1000.001 Hz on the 1 kHz CAL output).
- **Partial:** the oscilloscope loses samples at 16 MS/s (B-51, #14); 16 MS/s is best-effort.

## USB tree and port power (M1)

- The full hub topology as a tree, live on hot-plug (netlink, with a 1 Hz polling fallback).
- Per-port power on hubs with per-port power switching, through the kernel's sysfs `disable`
  attribute; no `uhubctl` binary.
- Ports are **discovered**, not configured. Protection against cutting a hub, a keyboard, a disk
  or Bluetooth is derived from what is attached; the user's lock/unlock choices persist across
  restarts (`state/usb-ports.json`).
- One-call power cycle that reports what enumerated back, and labels on ports, including marking
  the device under test (F-08).
- Every mutation from every transport is logged with its origin and shown in the web UI.

## Captures and measurements (M3, M4)

- Single captures held in a bounded in-memory store, addressed by `capture://` handles.
- Server-side measurements on full-resolution data: frequency, period, duty, pulse widths,
  rise/fall times, levels. Results carry their limits: frequency of a bursty line from the
  dominant population (B-39), rise times as upper bounds when an edge spans two samples (B-40),
  nothing timed inside the noise floor (B-04), clipping reported (B-54).
- `read_capture` returns min/max-decimated windows, capped in points; no tool returns bulk samples.

## Live recorder (M12)

- Continuous acquisition from both instruments into a bounded, compressed, in-memory history:
  run-length encoded digital, losslessly compressed analog blocks, and a multi-resolution summary
  tree. Ten minutes of the 1 kHz CAL signal at 1 MS/s take 9.1 MB.
- The browser follows a recording live at 30 fps with conflated view frames, and scrolls back
  through its history; the toolbar shows how much time the memory budget retains.
- Snapshots of any window become ordinary captures, usable with every capture tool.
- Recordings outlive the connection that started them; the Graph tab lists them on connect and
  can stop one it did not start (B-31).
- A digital recording survives a device stall: it resumes after the driver's reset, and the gap is
  listed and noted in results over it (B-25).
- **Partial:** an analog recording still ends on a stall, and views do not draw gaps yet.

## Pattern search and wait (M13)

A one-line pattern language for agents and the search box (SPEC §2.9): edge-anchored bit patterns
(`D0 f=2k 1010`), clocked patterns (`D1 clk=D0 rising 1010x`), edges, pulse widths, multi-channel
states and analog levels. `search_pattern` scans a recording's history in `O(edges)`, pruned by the
summary tree; `wait_for_pattern` resolves on the next match as it happens.

## Protocol decoders and channel survey (M5)

- Native streaming decoders: **UART** (with auto-baud per burst), **I²C** (ACK/NACK per
  transaction), **SPI**, **CAN** (classic and extended frames, from the analyzer or a differential
  scope pair), **1-Wire** (reset/presence, standard and overdrive, ROM commands with CRC).
- Decoders work on digital captures, on recordings, and on analog captures through a threshold
  front end with hysteresis.
- Output as annotations under the traces, or one entry per bus transaction, with a level filter;
  bursts and repeated transactions are folded (F-14).
- A one-call **channel survey** names UART, I²C and SPI lines, clocks and their rates, so an agent
  gets a decoder's channels and settings without trial and error.
- Everywhere: Graph tab Decode panel, live `decode` terminal source, MCP `decode`, SCPI `DEC:`.
- **Partial:** 1-Wire is tested only against synthesised waveforms; a run against a real DS18B20
  is open (#33).

## Rendered views (M16)

One server-side renderer behind MCP `render_view`, `capture.render` on the wire, `GET /api/render`
and the Graph tab's **Image** button, so a person and an agent see the same picture (SPEC §5.6):

| View | Answers |
|---|---|
| `waveform` | where in a capture something happens |
| `eye` | how repeatable a signal is: openings, duty, TIE and cycle-to-cycle jitter; NRZ data and periodic signals (B-74) |
| `histogram` | what a signal normally does: interval populations on a log axis |
| `anomalies` | which intervals do not fit, and where |

Every view returns `metrics` and time-located `findings` alongside the SVG (or PNG), plus a `url`
to fetch the same picture again (F-09).

## Terminal (M14)

An xterm.js tab with shared, server-side sessions on four sources:

- **Serial ports** through the host kernel's tty drivers: discovered, and surviving their USB port
  being power-cycled;
- the **SCPI console** on AWIC's own engine;
- the **server log** at info and above, plus every mutation with its origin, with a level filter;
- live **decoder output** from a recording.

MCP has the same serial ports: `serial_list`, `serial_open`, `serial_write`, `serial_read`,
`serial_expect`, `serial_close`.

## MCP server (M7) and monitor (M17)

- Streamable HTTP at `/mcp`, and `awic mcp-stdio` for stdio-only clients.
- 27 tools covering discovery, USB power, captures, measurements, decoding, the survey, views,
  recordings, pattern search and wait, and serial ports. `tools/list` is the authoritative
  catalog, with units, limits and an example in every description; `initialize` returns
  instructions describing the workflow and the channel numbering (F-15).
- Port protection and soft limits live in the core, so MCP inherits them with no bypass.
- The web UI's **MCP tab**: the endpoint and copyable VS Code, stdio and Claude Code settings;
  live sessions with call and error counts; and a log of every `/mcp` exchange with the pictures
  `render_view` sent, switchable and bounded in memory.

## SCPI server (M8)

TCP 5025, line-oriented. The IEEE 488.2 mandatory common commands and SCPI-99's required
subsystems (`SYST:ERR?`, `SYST:VERS?`, `STATus`) with the full status-reporting model, plus AWIC's
tree for instruments, USB power, acquisition, measurements, recordings and decoding. A generic VISA
client drives it with no AWIC-specific code; `make visa` runs 53 pyvisa checks against a live server
([tools/visa](../tools/visa/README.md)). `SYST:COMM:ERR:INL ON` turns a failed query into a
readable answer instead of a client timeout.

## Web UI (M2–M15)

- Vanilla TypeScript and web components, no framework. Tabs: USB, Graph, Terminal, MCP.
- Clicking **AWIC** in the header opens an About dialog: what AWIC is, a link to the project on
  GitHub, and GitHub Discussions as the place for questions, ideas and problems.
- Graph: capability-driven controls, Single and Live modes, cursors, measurements, decode panel,
  search box, Image button. WebGL2 renderer by default, Plotly selectable.
- Graph plot: wheel/pinch zoom, sideways swipe or Shift+wheel pan, drag, double-click fit; zoom and
  pan stay within the data. Volt grid and V/div per analog lane, a trigger marker (time and level)
  on triggered captures, labels that stay readable over traces, a hint on the empty plot. A new
  Single capture is always fitted into view; Live zooming keeps the old traces until new data
  arrives.
- Measurements grouped per capture with time, rate and length; earlier captures marked; SI
  prefixes and no more digits than the converter resolves.
- A toolbar that keeps its shape across states and modes; sample rates in MS/s.
- MCP log: *Follow* unticks when you scroll up to read and ticks again at the bottom.
- Compact layout: no page scroll at any window size on any tab; narrow windows stack or fold the
  side columns and compact the header. Light and dark theme.

## Security (M10)

- No authentication by default; an optional bearer token (`[auth] token`) protects both the UI and
  `/mcp`.
- `/ws` and `/mcp` refuse requests from foreign browser origins, so a visited web page cannot
  drive the bench.
- At most 32 MCP sessions, least recently used evicted.
- Port protection and soft limits are enforced in the core for every transport.

## Distribution

- Statically linked Linux executables (x86_64, aarch64, armv7) with the web UI compiled in, attached to each
  GitHub release with CycloneDX SBOMs and the third-party license notices
  ([DEVELOPMENT.md](DEVELOPMENT.md#6-releases)).
- A container image for rootless podman ([CONTAINER.md](CONTAINER.md)).
- Firmware from the host's `sigrok-firmware-fx2lafw` package, from `/etc/awic/firmware`, or from
  the container image.

---

## Planned

In value order within each group.

### Trustworthy results

- **Oscilloscope at 16 MS/s (B-51, #14):** find where samples are lost, and report a gap in the
  capture instead of hiding it.
- **Zero offset (B-14):** record each range's true zero code with the probe on its own ground,
  then drop the "offset not calibrated" note. Needs a multimeter reading of the CAL output as a
  reference (about 1.26 V DC if the scale is right).
- **Recorder stalls at 4 MS/s on the ESP32 bench**, and the other open items of #29 (B-94–B-98).
- **MCP2515 bench leftovers (#32):** SPI chip-select gaps below one sample, MISO in the survey,
  decode windows by decode position.
- **Analog recordings that survive a stall**, and gaps drawn in views.
- The remaining items of the four-model comparison (#2).

### PSU (M6)

A `PowerSupply` trait with a generic SCPI-over-serial template driver (Korad/RND KA3005P, Tenma
72-xxxx and similar) and a fake PSU; setpoints, output enable, a readback plot, and **soft limits
enforced in the core** for every transport. MCP `psu_set`/`psu_read`. Waiting for a PSU on the bench.

### Export (M9)

CSV, VCD, PNG, JSON and Parquet from every transport (SPEC §10); a VCD export opens in PulseView
or GTKWave with annotations intact. Its agent-facing half: an `export_capture` URL for a window at
full resolution (F-10).

### Analysis

- An SSD1306/SH1106 display decoder stacked on I²C (F-03).
- Step-response measurements: top/base, overshoot, settling time (F-11).
- More views: a level histogram ("3.3 V or 5 V, and is there a third state?"), cross-channel timing
  (skew, setup/hold), a mask/limit test with violation timestamps, a trend of one measurement over
  time, a spectrum (F-12).
- A "burst rate" measurement for signals that repeat in bursts (on the button's double-peaked
  burst `measure` reports the 41 Hz crossing rate, not the 28.38 ms repetition).
- Survey notes on glitch phase and under-sampling (F-16); range advice in analog results (F-13).
- Scope probe attenuation per channel (F-06); a correlated event timeline across serial, USB and
  recorder matches (F-07).

### Integration and UI

- Firmware auto-download (`[firmware] auto_download`): opt-in, pinned URLs, checked against
  committed SHA-256 sums (SPEC §13). The setting exists but is refused today.
- Debug probe integration via probe-rs: flash, reset, and RTT as a terminal source (F-01).
- Recordings on devices other than the selected one visible in the UI (`rec.list`, B-31 (c)).
- MCP tab: export the log as JSON lines with its pictures; link a log row's capture to the Graph
  tab; a colour per client.
- Re-run the four-model comparison (#2) and compare calls, errors, time and wrong
  facts against round 8's baseline: 15–74 calls and 5–33 minutes per model.
