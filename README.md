# Agentic Web Instrument Control (AWIC)

Browser, AI-agent and SCPI control for cheap USB bench instruments (oscilloscope, logic analyzer)
from one Rust server: native USB drivers, a vanilla-TypeScript web UI over WebSocket, an MCP server,
an SCPI socket, and USB hub port power switching.

> **First version, under development.** This is the first version of AWIC; it is still being
> developed and things will change. AWIC is developed entirely with
> [Claude Code](https://claude.com/claude-code).

**Status: in use on a real bench.** Logic analyzer, oscilloscope, decoders, live recorder, pattern
search, rendered views, MCP, SCPI and the terminal all work on hardware; a PSU module and export are
planned. Details in [docs/FEATURES.md](docs/FEATURES.md).
License: [MPL-2.0](LICENSE) — no GPL dependency.

## Hardware

AWIC drives typical low-cost USB instruments that [sigrok](https://sigrok.org) also supports, with
its own native drivers. It loads the same open firmware sigrok uses, from the
`sigrok-firmware-fx2lafw` package.

| Device | Supported models | ID | Confirmed |
|---|---|---|---|
| Logic analyzer | The common 8-channel, 24 MHz USB logic analyzers built on a Cypress FX2 and run with sigrok's fx2lafw firmware | `0925:3881` | 8 ch, D0-D7 |
| Oscilloscope | FX2-based 2-channel USB oscilloscopes that take sigrok's fx2lafw-derived firmware | `04b4:602a` → `1d50:608e` | 2 ch, 48/30/24/16/8/4/1 MHz, 500/200/100 kHz |

Which models these are: sigrok's device notes, under
[fx2lafw](https://github.com/miek/libsigrok/blob/master/README.devices#L40) for the logic
analyzers and [hantek-6xxx](https://github.com/miek/libsigrok/blob/master/README.devices#L44) for
the oscilloscopes, and the sigrok wiki's [fx2lafw](https://sigrok.org/wiki/Fx2lafw) and
[supported hardware](https://sigrok.org/wiki/Supported_hardware#Oscilloscopes) pages.

Some of these oscilloscopes also have a logic-analyzer mode, chosen with a physical switch. In logic
mode the oscilloscope appears as `0925:3881`, **the same ID as the standalone logic analyzer**, and
AWIC drives it as one. So AWIC identifies devices by USB bus path, never by VID:PID.

## Install

AWIC runs on **Linux x86_64**. Each [GitHub release](https://github.com/mrn2se/AgenticWebInstrumentControl/releases)
carries `awic-linux-x86_64`: one statically linked executable with the web UI compiled in, so it
needs no Rust, no Node and no container. To run AWIC in a container instead, see
[docs/CONTAINER.md](docs/CONTAINER.md); to build it yourself, [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md#7-building-and-running-from-source).

**1. Packages.** The firmware AWIC loads into the instruments, and a font for the labels in PNG
views (Debian and Ubuntu names):

```sh
sudo apt install sigrok-firmware-fx2lafw fonts-dejavu-core
```

`sigrok-firmware-fx2lafw` installs into `/usr/share/sigrok-firmware`. Without it, put the two `.fw`
files (`fx2lafw-saleae-logic.fw`, `fx2lafw-hantek-6022bl.fw`) into `/etc/awic/firmware`.

**2. USB access without root.** The udev rules give the `plugdev` group the instruments, the USB
hubs and the hubs' port-power switches:

```sh
curl -LO https://raw.githubusercontent.com/mrn2se/AgenticWebInstrumentControl/main/tools/udev/99-awic.rules
sudo cp 99-awic.rules /etc/udev/rules.d/ && sudo udevadm control --reload-rules && sudo udevadm trigger
sudo usermod -aG plugdev $USER   # then log out and back in, and replug the instruments
```

**3. Run.**

```sh
chmod +x awic-linux-x86_64
./awic-linux-x86_64
```

Open <http://127.0.0.1:8080/>. SCPI listens on port 5025. `--help` lists the options. Your USB port
locks are kept across restarts in `~/.local/state/awic` (or `$XDG_STATE_HOME/awic`). Distribute `THIRD-PARTY-LICENSES.txt` from the release along with the executable.

## Configuration

Every setting in [`config/awic.toml`](config/awic.toml) (download it as a starting point) can be
given three ways. Each one overrides the ones above it:

1. **The config file:** `--config PATH`, else `$AWIC_CONFIG`, else `/etc/awic/awic.toml`. Without
   one, AWIC uses its defaults. A file named by `--config` or `AWIC_CONFIG` must exist.
2. **Environment variables:** `AWIC_<SECTION>__<KEY>`, e.g. `AWIC_SERVER__BIND=0.0.0.0:8080`. A
   variable that names no setting is logged as a warning.
3. **Server arguments:** `--set section.key=value` (repeatable), or `--bind ADDR` for
   `server.bind`. An unknown key is refused.

Values parse like TOML (`true`, `5025`, `[a, b]`), otherwise as a plain string:

```sh
./awic-linux-x86_64 --config awic.toml --set recorder.memory_mb=512
AWIC_AUTH__TOKEN=secret ./awic-linux-x86_64 --bind 0.0.0.0:8080
```

The server binds `127.0.0.1:8080` by default, reachable from this machine only. Read
[Security](#security) before binding it to anything else.

## Using AWIC from an AI agent

AWIC exposes an [MCP](https://modelcontextprotocol.io) server alongside the web UI and the SCPI
socket — the same core, the same port protection, the same mutation log (every agent action shows
up in the web UI's `log` terminal with its origin). Tools cover discovery, USB port power, captures,
measurements, decoding, rendered views, the recorder's pattern search and wait, and serial ports;
`tools/list` is the full catalog, with units, limits and an example in every description.

The web UI's **MCP** tab shows the endpoint, the settings to paste into VS Code or Claude Code, the
sessions connected, and a live log of every MCP request and response, including the pictures
`render_view` sent.

**HTTP.** Point any client that speaks Streamable HTTP at `http://<host>:8080/mcp`. With
`[auth] token` set, send it as `Authorization: Bearer <token>`:

```json
{
  "mcpServers": {
    "awic": {
      "url": "http://192.168.1.50:8080/mcp",
      "headers": { "Authorization": "Bearer <token>" }
    }
  }
}
```

**stdio.** `awic mcp-stdio` is a proxy in the same binary: newline-delimited JSON-RPC on
stdin/stdout, forwarded to a running server's `/mcp`:

```json
{
  "mcpServers": {
    "awic": {
      "command": "/path/to/awic",
      "args": ["mcp-stdio", "--url", "http://127.0.0.1:8080/mcp"],
      "env": { "AWIC_MCP_TOKEN": "<token if set>" }
    }
  }
}
```

(`AWIC_MCP_URL` works in place of `--url`.)

A typical session — find something in a live signal without pulling raw samples over the wire:

```jsonc
list_instruments()
// -> { "devices": [{ "id": "3-3.4.1.1", "label": "...", "capabilities": [{"kind":"digital", ...}] }] }
start_recording({ "device": "3-3.4.1.1", "rate_hz": 1000000, "channels": [0] })
// -> { "rec": 1, "cfg": { "rate_hz": 1000000, "continuous": true, ... } }
search_pattern({ "rec": 1, "pattern": "D0 f=2k 1010", "max_matches": 16 })
// -> { "matches": [{ "t_s": 0.000345, "sample": 345 }, { "t_s": 0.002345, ... }], "truncated": false }
snapshot_recording({ "rec": 1, "t0_s": 0.000345, "t1_s": 0.010345 })
// -> { "capture": "capture://..." }
measure({ "capture": "capture://...", "channel": 0, "measurements": ["frequency"] })
// -> { "results": [{ "kind": "frequency", "value": 1000.0, "unit": "Hz" }] }
stop_recording({ "rec": 1 })
delete_recording({ "rec": 1 })   // frees its memory; snapshots are kept
```

## Views an agent (or a person) can look at

`render_view` over MCP, `GET /api/render` in a browser and the Graph tab's **Image** button go
through one server-side renderer, so the picture a person shares and the picture an agent receives
are the same:

```
GET /api/render?capture=capture://...&kind=eye&channels=0
```

| `kind` | Shows | Answers |
|---|---|---|
| `waveform` | amplitude against time, every column the min/max of the samples behind it | where in a capture something happens |
| `eye` | every period of one channel folded and shaded by density | how repeatable the signal is — jitter, overshoot, a slow edge |
| `histogram` | the distribution of interval lengths on a log axis, with populations named | what the signal *normally* does, and what else is in there |
| `anomalies` | the intervals that do not fit, and where they are | which timestamps are worth looking at |

Every view returns its numbers as well as the picture, plus `findings` located in time. One second
of an SSD1306's I²C clock at 24 MS/s — 21 046 periods — comes back as five lines:

```
population_1  1.13 us  99.4 %   the clock
population_2  5.42 us   0.6 %   held longer between bytes and at the ACK
population_3  7.67 us   0.0 %
population_4  9.85 us   0.0 %
population_5   471 ms   0.0 %   the gap between display frames
```

## Using AWIC from a VISA client

The SCPI socket (TCP **5025**) is driveable from any standard VISA stack — no AWIC-specific client
code, no NI-VISA install:

```python
import pyvisa
instr = pyvisa.ResourceManager("@py").open_resource("TCPIP::<host>::5025::SOCKET")
instr.read_termination = "\n"          # the write side can stay at pyvisa's default "\r\n"
print(instr.query("*IDN?"))            # AWIC,awic,0,0.1.0
print(instr.query("INST:CAT?"))        # "3-3.4.2","3-3.4.1.1"
instr.write('INST:SEL "3-3.4.2"')
instr.write("ACQ:RATE 8E6; :ACQ:CHAN (@1); :ACQ:RANG 2.5; :ACQ:SAMP 100000")
instr.write("SYST:COMM:ERR:INL ON")    # so a failed query answers instead of timing out
print(instr.query("ACQ:STAR"))         # capture://...
print(instr.query("MEAS:FREQ? CH1"))   # 1.0002E+3
```

The IEEE 488.2 mandatory command set and SCPI-99's required subsystems are all there, so a generic
driver's startup sequence works as written. [tools/visa](tools/visa/README.md) checks conformance
against a live server.

## Security

**AWIC has close to no security features, by the nature of the project.** It is a bench tool that
hands control of physical hardware to whoever reaches it — a browser, a script or an AI agent. You
are responsible for where and how it runs.

- **Network.** Anyone who reaches port 8080 or 5025 can switch USB power, drive the instruments and
  write to serial ports. There is no authentication by default, no encryption (plain HTTP, plain
  TCP for SCPI) and no user management. Keep the default `127.0.0.1` bind, or bind only to a
  network you trust. **Never expose AWIC on an unprotected network or the internet.** The optional
  `[auth] token` is a shared secret sent in clear text; it keeps out casual access, not an attacker
  on the same network. `/ws` and `/mcp` refuse requests from foreign browser origins, so a web page
  you visit cannot drive the bench, but that is not authentication either.
- **Hardware safety.** AWIC switches USB port power and writes to whatever is attached, also when an
  AI agent decides to. Port protection (ports that feed a hub, a keyboard, a disk or Bluetooth are
  locked by default) reduces mistakes; it does not make it safe. Make sure nothing on the bench can be damaged,
  or do damage, when a port is switched off or on, a device is reset, or a serial port receives
  unexpected data. Do not connect AWIC to equipment where that matters. The instruments are hobby
  grade and the measurements are not calibrated.
- **What runs.** The server runs with your user's rights and, with the udev rules, can write to the
  USB devices and hub ports the rules name. The firmware it loads into the instruments is executed
  by them; it comes from the Debian package or your own `/etc/awic/firmware`. AWIC downloads nothing.
- **Dependencies.** The release is built by GitHub Actions from the tagged source. Every Rust crate
  and npm package comes from crates.io and the npm registry at the exact version recorded in
  `Cargo.lock` and `web/package-lock.json`, built with `--locked`/`npm ci`, and listed with its
  license in the release's SBOMs (`*.cdx.json`) and `THIRD-PARTY-LICENSES.txt`. A release is only
  built when `cargo audit` and `npm audit` report no known advisory and every npm package's
  registry signature verifies ([docs/DEVELOPMENT.md](docs/DEVELOPMENT.md#6-releases)). This rules
  out known problems, not unknown ones.

Report security problems through a [GitHub issue](https://github.com/mrn2se/AgenticWebInstrumentControl/issues).

## No warranty

AWIC is provided **"as is", without warranty of any kind**, express or implied, including fitness
for a particular purpose, as stated in sections 6 and 7 of the [MPL-2.0](LICENSE). You use it at
your own risk; the authors are not liable for damage to hardware, devices under test, data or
anything else that results from its use.

## Documentation

| | |
|---|---|
| [docs/FEATURES.md](docs/FEATURES.md) | What AWIC does, what is partial, what is planned |
| [docs/SPECIFICATION.md](docs/SPECIFICATION.md) | How it works: drivers, recorder, protocol, decoders, views, MCP, SCPI; the design decisions (§15) |
| [docs/CONTAINER.md](docs/CONTAINER.md) | Building and running the container image |
| [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) | Building from source, code conventions, crate layout, tests, releases |
| [example-report/](example-report/README.md) | Example: a live bench surveyed over MCP, with server-rendered views and MCP tab screenshots |
| [tools/](tools/README.md) | Helper scripts outside the server: container runners, udev rules, VISA check, license notices |
