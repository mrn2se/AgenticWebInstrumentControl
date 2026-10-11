# AWIC — Technical Specification

> **Revision 5** (2026-10-11). How AWIC works. What it does, and what is planned, is in
> [FEATURES.md](FEATURES.md); the design decisions behind it, with the IDs (Q-, R-, D-, S-, N-)
> used throughout this document and the code, are in §15.
>
> **License: MPL-2.0** (file-level copyleft, D-11). There is no GPL dependency. GPL-2.0+ FX2
> firmware blobs ship alongside as runtime data, which is aggregation, not derivation.

---

## 1. Container

### 1.1 Base image

`debian:trixie-slim`, two stages. With libsigrok gone there is **no C library in the process at
all**, so the runtime image is a Rust binary, the built frontend, and firmware blobs.

| Stage | Image | Produces |
|---|---|---|
| `web` | `node:22-trixie-slim` | `/web/dist` — Vite production build |
| `build` | `rust:1.98-slim-trixie` | `/usr/local/bin/awic` |
| runtime | `debian:trixie-slim` | binary, `dist/`, firmware blobs |

Runtime packages: `ca-certificates` and `sigrok-firmware-fx2lafw` (blobs only — the library is not
installed). That is the whole dependency list.

Verified available in trixie: `sigrok-firmware-fx2lafw` 0.1.7-3, which ships
`fx2lafw-hantek-6022bl.fw` and `fx2lafw-saleae-logic.fw` — blobs for both of your instruments.

### 1.2 Host setup — udev rules (Q-01)

Measured: `/dev/bus/usb/003/*` is `crw-rw-r-- root root`; user `coder` has **read only**, and USB
device access needs **write**. Rootless podman `--privileged` does not change host file ownership,
so it cannot fix this. The user is already in `plugdev`.

`udev/99-awic.rules`, installed once with sudo:

```udev
# FX2 oscilloscope — scope mode, before and after firmware upload
SUBSYSTEM=="usb", ATTR{idVendor}=="04b4", ATTR{idProduct}=="602a", MODE="0660", GROUP="plugdev"
SUBSYSTEM=="usb", ATTR{idVendor}=="1d50", ATTR{idProduct}=="608e", MODE="0660", GROUP="plugdev"
# fx2lafw logic analyzer — and the oscilloscope when its physical switch is in logic position (§5.1)
SUBSYSTEM=="usb", ATTR{idVendor}=="0925", ATTR{idProduct}=="3881", MODE="0660", GROUP="plugdev"

# Hubs — port power control talks to the HUB, not to the device
SUBSYSTEM=="usb", ATTR{idVendor}=="2109", ATTR{idProduct}=="2822", MODE="0660", GROUP="plugdev"
SUBSYSTEM=="usb", ATTR{idVendor}=="0bda", ATTR{idProduct}=="5411", MODE="0660", GROUP="plugdev"
```

The hub rules are the part that is easy to forget: switching port power is a control transfer to
the **hub**, so the hub's own node must be writable. Granting only the instrument gets you a
working scope that cannot be power-cycled.

Both oscilloscope IDs are listed because the device re-enumerates from `04b4:602a` (unprogrammed) to
**`1d50:608e`** once sigrok's firmware is loaded — the generic fx2lafw ID, with iSerial
"FX2 oscilloscope" (observed with `lsusb -v`; an earlier draft wrongly assumed `04b5:602a`). It must
be writable in both states.

Rootful `sudo podman run --privileged` stays documented as the fallback (Q-01 option C). **This is
the path that was used to verify the hardware end to end** (§15, R-02).

### 1.3 Runtime invocation

```bash
podman run --rm --name awic \
  --net=host \                              # uevent netlink is netns-scoped (§4.3)
  --group-add keep-groups \                 # carries plugdev into the container
  --security-opt apparmor=unconfined \      # Ubuntu 26.04 AppArmor is enforcing
  -v /dev/bus/usb:/dev/bus/usb \            # bind mount, NOT --device (§1.4)
  -v /sys/devices:/sys/devices \            # writable: port power via sysfs (§4.2)
  -v /dev:/host/dev \                       # UART tty nodes, tracked live (§11.2.1)
  -v ./config:/etc/awic:ro \
  -v ./state:/var/lib/awic \
  localhost/awic:dev
```

The `/dev` bind is for serial consoles: USB-serial tty nodes appear and disappear with their port
power, exactly like `/dev/bus/usb` nodes (§1.4), so a per-node `--device` would go stale. Verified in
rootless podman: `/host/dev/ttyACM0` opens read-write through `keep-groups`. Rootless, the process
still has only the bench user's permissions on it.

### 1.4 `--device` vs bind mount

`--device /dev/bus/usb/003/010` injects **one node, once**. Both a power cycle *and* a firmware
upload make the device re-enumerate with a **new device number**, and the injected node goes
stale. The whole-directory bind mount is required so the container tracks host devtmpfs live.
This was confirmed during verification: the LA moved from device 011 to 012 across its firmware
upload, and the bind mount followed it.

---

## 2. Backend

### 2.1 Crates

| Concern | Choice | Note |
|---|---|---|
| Runtime | `tokio` (multi-thread) | |
| HTTP/WS | `axum` 0.8 + `tower-http` | serves SPA, `/ws`, `/mcp` |
| USB | **`nusb`** | pure Rust; `rusb` is the fallback (§15, R-02) |
| Serial (UART, PSU) | `rustix` termios + tokio `AsyncFd` | pure Rust; host tty nodes (§11.2.1) |
| Terminal (web) | `@xterm/xterm` | §11.2 |
| Serialization | `serde_json` (control), raw binary (samples) | |
| TS types | `ts-rs` | generates `web/src/proto/types.ts` from Rust |
| MCP | hand-rolled JSON-RPC | streamable HTTP, protocol 2025-06-18; `rmcp` was evaluated and set aside — its handler stack would sit beside dispatch rather than on it (§8.1) |
| Parquet | `arrow` + `parquet` | Q-15 |
| Logging | `tracing` | |
| Config | `figment` (TOML + env) | |

### 2.2 Backend abstraction

Requested in R-02 and now load-bearing rather than aspirational:

```
awic-core        trait Backend          ← names no driver type, ever
   ├── awic-native    impl Backend      fx2lafw + fx2scope   (primary)
   └── awic-fake      impl Backend      synthetic signals      (always built)
```

`awic-core` must not name a driver-specific type in any signature, error enum, or config struct.
The test that this holds: a third backend could be added without touching core.

### 2.3 Device abstraction

```rust
/// Identity that survives re-enumeration. Device NUMBERS change on every power
/// cycle AND every firmware upload, so the bus path is the stable key.
/// (libsigrok addressed our oscilloscope as "3.13" — bus.devnum — which is exactly
/// the fragility we are avoiding.)
pub struct DeviceId {
    pub bus: u8,
    pub port_path: Vec<u8>,   // [3, 4, 2] == "3-3.4.2"
    pub vid: u16,
    pub pid: u16,
}

pub enum Capability {
    Analog  { channels: u8, bits: u8, rates_hz: Vec<u64>, ranges_v: Vec<f32> },
    Digital { channels: u8, max_rate_hz: u64, rates_hz: Vec<u64> },  // exact ladder, like Analog
    PowerSupply { channels: u8, v_max: f32, i_max: f32 },
}

#[async_trait]
pub trait Instrument: Send + Sync {
    fn id(&self) -> &DeviceId;
    fn capabilities(&self) -> &[Capability];
    /// What this is in a person's words ("FX2 oscilloscope, 2-channel
    /// The oscilloscope") — `device.list`'s `label`. A bus path alone tells
    /// neither a user nor an agent which box they are holding.
    fn label(&self) -> Option<String> { None }

    /// Runs on EVERY enumeration. FX2 devices hold firmware in RAM and lose it
    /// on power loss, so this is not an init-once hook.
    async fn prepare(&mut self) -> Result<()>;

    /// Returns what the device will ACTUALLY do (§2.3.1).
    async fn configure(&mut self, cfg: AcqConfig) -> Result<EffectiveAcq>;
    async fn start(&mut self, sink: SampleSink) -> Result<CaptureId>;
    async fn stop(&mut self) -> Result<()>;
}
```

There is deliberately **no `set_mode()`**. The oscilloscope's scope/logic selection is a physical switch
(§5.1); AWIC detects and reports the mode but cannot change it.

### 2.3.1 Effective configuration and calibration

`configure()` returns an `EffectiveAcq { cfg, calibration }` — what the device will actually do —
and `acq.configure` sends it to the client. The request is never echoed back as if it were fact:

- **The rate may differ from the request.** The FX2 oscilloscope has a fixed rate ladder; a request
  for 10 MHz runs at a neighbouring rate. A client that kept its own value would build a timebase
  off by the difference.
- **Calibration comes from the driver**, per channel: `value = raw * scale + offset`, plus unit
  and the ± range actually applied. Only the driver knows the ADC width, the front-end gain per
  range and per-range calibration offsets. An earlier client-side derivation from bit depth read
  the protocol's ± range as peak-to-peak and drew every trace at half amplitude.
- **Ranges are requestable**: `AcqConfig.ranges_v` carries the wanted ± full-scale range per
  enabled channel, from the device's `Capability::Analog.ranges_v`. Omitted on the wire when
  empty, so a digital request is unchanged.

The server files captures with this calibration, so the store holds raw codes plus the exact
conversion the hardware implies.

### 2.3.2 Continuous acquisition

`AcqConfig.continuous = true` makes a driver stream until `stop()`; `samples` is ignored
(omitted on the wire when false). This is what the recorder (§2.8) runs on. Everything else is
unchanged: multiple transfers in flight, the stall timeout still ends an acquisition whose device
went quiet (§4.7), and `t0_ns` of each chunk keeps counting from the start. The recorder derives
sample indices by summing `sample_count` — exact, because the driver-facing sink never drops
(§2.6) — and never reconstructs them from nanoseconds, which round.

### 2.4 FX2 firmware upload (shared by both drivers)

Both instruments are Cypress EZ-USB FX2LP devices that enumerate with no firmware and must be
programmed over USB on every appearance. One shared module, ~150 lines:

```
1. Halt the 8051:     vendor write 0xA0, wValue = 0xE600 (CPUCS), data = [0x01]
2. Upload the image:  vendor write 0xA0, wValue = <load address>, in ≤4096-byte chunks
                      (the sigrok .fw blobs are raw binary loaded from 0x0000)
3. Release reset:     vendor write 0xA0, wValue = 0xE600, data = [0x00]
4. Device disconnects and re-enumerates with its programmed PID. Wait for it by
   BUS PATH, not by device number.
```

Verified working from inside a container during hardware validation — the fx2lafw logic analyzer
re-enumerated as `0925:3881 sigrok fx2lafw` after upload. This was the single riskiest assumption
in the whole plan and it holds.

### 2.5 Native drivers

Protocol details are taken from the reverse-engineering documentation and cross-checked against
the reference implementations (`Fx2ScopeAPI`'s `REVERSE_ENGINEERING.md`, OpenFx2Scope, and
sigrok's own drivers as a behavioural oracle). **Exact command bytes are validated against the
hardware during M3/M4 rather than trusted from documentation** — the ground truth we already have
is recorded below.

**`fx2lafw` driver — fx2lafw logic analyzer, 8 digital channels**

Command set on endpoint 0 (`0xB0` get firmware version, `0xB2` get revision, `0xB1` start
acquisition with a flags + sample-delay struct), then continuous bulk IN on `0x86`. Samples are
raw bytes, one bit per channel. Sample rate is expressed as a delay value; the rate ladder must
match what the firmware actually accepts.

**`fx2scope` driver — FX2 oscilloscope, 2 analog channels**

Established from ground truth — sigrok's own traffic captured with usbmon (`tcpdump -i usbmon3`;
the debugfs text interface is blocked by kernel lockdown, the binary `/dev/usbmonN` is not) and
compared packet by packet with this driver's. Several facts differ from the documentation first
assumed:

| Request | Purpose |
|---|---|
| `0xE0` / `0xE1` | CH1 / CH2 gain: code 1, 2, 5, 10 = ±5, ±2.5, ±1.25, ±0.5 V |
| `0xE2` | sample rate (MHz value for 48…1 MHz; kHz/10 for 500/200/100 kHz) |
| `0xE3` | start (1) / stop (0) |
| `0xE4` | **never written**: the device always streams both channels, interleaved |
| `0xE5` | coupling; there is no `0xE6`, and no on-device calibration is readable |

- **Every value travels in the control transfer's data stage** (one byte, wValue 0). With the
  value in wValue the firmware configures nothing and never starts.
- **No factory calibration is reachable** with this firmware: the FX2-convention EEPROM read
  (`0xA2`) stalls. Zero offsets are therefore measured (see §2.5.1).
- Start sequence: registers, then **submit the bulk reads, then start** — the reverse leaves the
  2 KB FIFO without a pending read and overruns at 16 MHz. Discard the first 2080 bytes: the FIFO
  plus a 10-byte internal pipeline, measured, holding samples from before the start.
- **Highest sustained rate: 16 MHz** (32 MB/s, both channels always on the wire). Above it the
  device delivers ~22.5 MS/s — the bus ceiling — and drops samples *evenly*: a 24 MHz capture looks
  continuous while its timebase is 6.5 % off. Refused at `configure()`.

Verified against the front-panel CAL square wave: 999.998 / 1000.001 / 1000.001 Hz at 1 / 8 / 16 MHz
over 499 periods, shortest period within 6 samples of the mean (no lost samples).

#### 2.5.1 Zero offset — open

Measured with CAL's 0 V level, 0 V sits at code 136 / 139 / 146 / 155 on the four gains instead of
128: an offset of roughly 6 codes plus ~2 per gain step, so at ±5 V a 0 V input reads +0.33 V. The
gain *steps* are right (a signal's step scales 1 : 2.2 : 5.1 : 10.4). Correcting it needs a stored
per-channel, per-gain zero point measured with the input at 0 V. Absolute gain additionally needs an
external amplitude reference.

**Verified ground truth to reproduce** (from the hardware validation run):

```
FX2 oscilloscope   2 channels: CH1 CH2
                rates: 48, 30, 24, 16, 8 (default), 4, 1 MHz; 500, 200, 100 kHz
fx2lafw logic analyzer    8 channels: D0–D7
```

A native driver that does not reproduce exactly this capability set is wrong, and we can tell
immediately rather than at the end of M4.

### 2.6 Concurrency

Each device is owned by one async task holding its `nusb` interface. Streaming uses `nusb`'s
`Queue` with several bulk transfers in flight — at 48 MS/s a single outstanding transfer will
drop samples. Commands reach the task over `tokio::sync::mpsc`; samples leave over a bounded
channel that applies backpressure (§3.2).

No dedicated OS threads, no C callbacks, no GLib loop. Dropping libsigrok removed an entire class
of threading hazard rather than mitigating it.

### 2.7 Capture store

- Ring of the last **N** captures (default 32) or **M** MB (default 512), whichever binds first.
- In-memory only (Q-16). Explicit export is how anything survives a restart.
- Analog stored as **raw device samples plus scale/offset**, not pre-converted floats —
  measurements must run on the values the hardware produced.
- Digital stored as packed bits **and** a derived transition list, built once and consumed by both
  the decoders (§6) and VCD export (§10).
- Addressed as `capture://<uuid>`.

### 2.8 Recorder — continuous, compressed, searchable (M12)

A **recording** is a continuous acquisition (§2.3.2) from one device into a bounded, compressed,
in-memory history that every transport can view, search and wait on while it grows. It
complements the capture store: a capture is a finite full-resolution snapshot, a recording is a
rolling window — and any range of a recording can be snapshotted into a capture (§2.8.5), so
measurements, decoders and export apply to it unchanged.

- **Server-owned, not per-connection.** The browser and an agent see the same recording, so an
  agent can wait on a pattern while a human watches the live plot. It keeps running when the tab
  that started it closes; it stops on `rec.stop`, on device loss, or on a driver error.
- **One recording per device.** While a device records, `acq.start` on it is refused with
  `device_busy`, and `acq.configure` too — reconfiguring would silently change a running
  recording's timebase.
- **One timebase per recording.** Absolute sample index `u64` from the start; `t = index / rate_hz`.
  A new rate, channel set or range is a new recording. A recording never mixes rates.
- Crate **`awic-record`**: pure data structures and algorithms, no I/O, no tokio. The server owns
  the tasks (§3.4).

Addressed as `rec://<n>` with a small integer id, listed by `rec.list`.

#### 2.8.1 Digital: run-length encoded

The packed sample stream (one byte per sample, bit *n* = D*n*) is stored as **runs** — the
transition list of §2.7 under another name: a run is `(value u8, length)` and ends whenever the
packed byte changes.

- Runs are grouped in **blocks** of up to 4096 runs, each encoded as `value u8, length LEB128`,
  with a block index of first sample indices for `O(log n)` seek by time.
- Cost is proportional to **edges, not samples**. A 1 kHz square wave on D0 sampled at 1 MHz is
  2 runs per ms, about 6 bytes/ms against 1000 bytes/ms packed — ~170× smaller. An idle channel
  costs nothing per second.
- The worst case (a change on every sample) would cost ~2 bytes/sample. A block whose runs would
  exceed its packed size stores the packed bytes instead; the block header says which, so the
  pathological case costs 1×, never 2×.

#### 2.8.2 Analog: raw codes in losslessly compressed blocks

Raw ADC codes — never volts (§2.7) — in blocks of 64 Ki samples per channel. Each block names its
codec in its header, so a better codec is a new id rather than a format break:

| Id | Codec |
|---|---|
| 0 | stored raw |
| 1 | delta, zigzag, then bit-packed at the block's widest delta, with an escape for outliers |

The encoder keeps whichever is smaller per block. Lossless, because a snapshot of a recording must
measure exactly what the hardware produced. Pure Rust, no C (§1.1). The ratio actually achieved is
reported by `rec.list` rather than promised here: noise dominates 8-bit scope data, so expect
roughly 2× on a live signal and far more on a flat one.

#### 2.8.3 Summary tree

Both kinds keep a **multi-resolution summary**: a pyramid of buckets over sample index.
Level 0 buckets span `2^k` samples, the power of two nearest `rate_hz / 1000` (≈ 1 ms, at least
64); each higher level merges 8 buckets of the level below. Per bucket:

| Kind | Summary |
|---|---|
| digital | `AND` and `OR` of the packed byte — per bit: low throughout, high throughout, or toggled (§3.2 encoding 5); per channel, the **high-sample count** (so the fraction high — "average 0.7" — of any span) and the **edge count** |
| analog | per channel `min`, `max`, and `sum` of raw codes (so the mean of any span) |

The tree is appended incrementally, so its top is always current. Because level 0 is sized in
time, summary memory is ~independent of the sample rate; resolution finer than level 0 is computed
on demand from the runs or the raw block, which inside a small window is cheap by construction.

The tree answers "what happened between t0 and t1" in time proportional to the answer, not the
data: drawing a 10 s window at 24 MS/s reads ~2 k buckets, not 240 M samples. It also **prunes
searches** (§2.9): a pattern that needs an edge on D3 cannot match inside a bucket whose D3 AND
equals its OR, and a level trigger at 1.5 V skips every bucket whose max is below it.

#### 2.8.4 Memory budget and retention

`[recorder] memory_mb` (default 256) bounds all recordings together. Over budget, the oldest
blocks and their summary buckets are evicted; a recording's `first_sample` advances and clients
see the retained span shrink from the left. `rec.list` reports per recording: rate, channels,
retained span in seconds, bytes used, and the compression ratio against packed/raw size.

Eviction and the tree's growth interact: when a group of 8 buckets completes after its oldest
members were evicted — typically as a long recording grows a new top level, whose first bucket
would cover everything since sample 0 — no parent is built for it. A parent summarizes all 8
children or does not exist; the queries above already fall back to finer levels where one is
missing. A panic inside the recorder halts only that recording, with an error state, and never
the others (live test, B-01).

At 1 MS/s with a few slow digital channels the budget holds hours; at the oscilloscope's 16 MS/s,
two noisy channels, it holds tens of seconds. That asymmetry is the point of run-length encoding
the logic side.

#### 2.8.5 Snapshots

`rec.snapshot {rec, t0_s, t1_s}` decompresses a range into an ordinary capture (§2.7) and returns
its `capture://` handle, capped at 64 Mi samples. The capture carries the recording's calibration
and channel list, so `capture.measure`, decoders and export work on it with no special case.

### 2.9 Pattern search and wait (M13)

The agent-facing half of the recorder. An event is described in one short, readable line; AWIC
**searches** a recording's history for it, or **waits** until it happens. Implemented in crate
`awic-pattern` (parser and matchers over run and sample iterators, no storage knowledge), used by
`awic-record`.

#### 2.9.1 Syntax

```
D0 f=1000 100010x101     bit pattern on D0 at 1000 bit/s, x = don't care
D0 f=1k, 100010x101      same; SI suffixes k M, commas optional, case-insensitive
D2 rising                any rising edge        (also: falling, edge)
D1 high >2ms <5ms        a high pulse whose width is in range; either bound optional
D1 low >=10ms            a low level held at least 10 ms
D3..D0 = 1x01            multi-channel state, MSB first, becoming true
D1 clk=D0 rising 1010x   D1 read at each rising edge of D0 (synchronous data; S-02)
CH1 rising 1.2V          analog crossing a level upward (also: falling, cross)
CH1 >1.5V                analog above a level     (also: <)
```

Channel names follow the device: `D0`–`D7` digital, `CH1`, `CH2` analog, matching the UI labels.
The channel may be omitted when the recording has exactly one digital channel (`f=1000,
100010x101` then means that channel); with several, the parser asks for one rather than guess.
A pattern that names a channel the recording lacks is refused at parse time, with the list of
channels it has. Parse errors point at the offending token — an agent should be able to fix its
own pattern from the message alone.

#### 2.9.2 Bit-pattern semantics

Edge-anchored, like a UART receiver, because there is no shared clock to sample against:

1. Each run of the channel with duration `d` covers `n = round(d·f)` bit cells.
2. A run shorter than half a cell (`n = 0`) is a **glitch**; it is absorbed into its
   neighbours, and the result reports how many were absorbed.
3. If `|d·f − n|` exceeds the tolerance (`tol=`, default 0.3 cells), the run is **off-rate** and no
   match may span it — noise must not manufacture matches.
4. The cell sequence is searched for the pattern; `x` matches either level. The reported time is
   the start of the first matched cell (run start + `k/f`).
5. Matches are non-overlapping, earliest first; inside one run longer than the pattern, only the
   first position is reported — it is one event, not thousands.

Searching iterates runs, so its cost is `O(edges)`, not `O(samples)`.

#### 2.9.3 Operations

```jsonc
{ "op": "rec.search", "rec": 1, "pattern": "D0 f=1k 100010x101",
  "t0_s": 0.0, "t1_s": 12.5, "max_matches": 16 }
→ { "matches": [ { "t_s": 3.2140, "sample": 3214000 } ], "truncated": false,
    "searched": { "t0_s": 0.0, "t1_s": 12.5 }, "glitches": 0 }

{ "op": "rec.wait", "rec": 1, "pattern": "D2 rising", "timeout_ms": 10000 }
→ { "match": { "t_s": 41.0021, "sample": 41002100 } }   // or { "timeout": true }
```

- `max_matches` defaults to 16, hard maximum 1024 — the context budget (§8.3) applies.
- `t0_s`/`t1_s` default to the retained span. Searching evicted history is an error that names
  the retained span, not an empty result.
- Over a recording that is no longer live, the result (and `rec.decode`'s, and `survey`'s) carries
  a `note` saying when it ended and why — `stopped at 43.600 s`, or the error that ended it.
  An empty result there covers only what was recorded, and must not read as "nothing happened".
- `rec.wait` resolves with the first match **after the request arrives** (or after `from_s`).
  It runs on the recorder's append path: each new chunk is searched from the last checked
  position minus the pattern's longest span, so a match straddling two chunks is found exactly
  once. `timeout_ms` is capped at 600 000. The wait ends with an error if the recording stops.
- Both are exposed as MCP tools (`search_pattern`, `wait_for_pattern`) in M7, returning the same
  shapes.

---

## 3. Wire protocol (WebSocket)

One socket at `/ws`. Text frames are JSON control; binary frames are samples.

### 3.1 Control

```jsonc
// client → server
{ "v": 1, "id": 42, "op": "device.list" }
{ "v": 1, "id": 43, "op": "usb.tree" }
{ "v": 1, "id": 44, "op": "usb.power", "hub": "3-3", "port": 3, "on": false }
{ "v": 1, "id": 45, "op": "acq.configure", "device": "3-3.4.2",
  "cfg": { "rate_hz": 24000000, "channels": [0,1], "samples": 1048576,
           "trigger": { "kind": "edge", "channel": 0, "slope": "rising", "level": 1.5 } } }

// server → client
{ "v": 1, "id": 42, "ok": true, "result": { "devices": [ ... ] } }
{ "v": 1, "id": 44, "ok": false, "error": { "code": "port_protected", "msg": "..." } }
{ "v": 1, "event": "device.added",  "device": { ... } }
{ "v": 1, "event": "device.state",  "device": "3-3.4.2", "state": "flashing" }
{ "v": 1, "event": "acq.state", "stream": 7, "state": "armed" }
```

Every request carries `id`; every response echoes it; events never carry one.

A trigger's `level` is volts and only means something on an analog channel. A digital trigger
leaves it out — its edge is a change of the channel's bit (B-96: the schema once required it, and
a dummy `level: 0` made the fake analyzer's trigger never fire) — and an analog trigger without
one is refused at `acq.configure`, naming the channel, rather than defaulting to 0 V.

A request that cannot be parsed still gets a response — the connection is never closed over one
(live test B-27) — and the code says which kind of mistake it was:

| `code` | When | Also in `msg` |
|---|---|---|
| `invalid_request` | the frame is not JSON | the parse error; `id` is `0`, since none could be read |
| `unknown_op` | no such `op`, or no `op` at all | **every op that does exist**, listed |
| `invalid_params` | the `op` exists, its fields do not fit | which field, by name |

The split matters for agents: answering `unknown_op` to a known op with a mistyped field tells a
caller that the operation does not exist, so it goes looking for another way to do the thing
instead of fixing its arguments.

### 3.2 Sample frames (binary)

24-byte little-endian header, then payload:

```
off  size  field
 0    1    magic 0xA5
 1    1    version (1)
 2    2    stream_id        u16
 4    4    sequence         u32    per stream; detects drops
 8    1    encoding         u8     see encoding table below
 9    1    channel_count    u8
10    2    reserved
12    4    sample_count     u32    per channel
16    8    t0_ns            u64    capture-relative
24   ..    payload
```

| Value | Encoding | Payload layout |
|---|---|---|
| 0 | `u8` interleaved | raw 8-bit samples, channels interleaved |
| 1 | `i16` | raw little-endian `i16`, channels interleaved |
| 2 | packed digital | one byte per sample; bit *n* = channel D*n* |
| 3 | min/max `u8` | decimated; **channel-major**, per channel `buckets` × `(min u8, max u8)` |
| 4 | min/max `i16` | decimated; channel-major, per channel `buckets` × `(min i16 LE, max i16 LE)` |
| 5 | bitwise digital | decimated; **one stream**, `buckets` × `(AND u8, OR u8)` |

Values 3–5 are decimated previews, and they are deliberately distinct. An earlier draft had a
single "min/max pairs" value, which left a decoder unable to recover the sample width or the
semantics from the frame. Every frame must be decodable from its own header.

**Digital data is decimated bitwise, never by numeric min/max.** A packed byte is a bitmask, not a
number: the numeric min/max of `0x01` (D0 high) and `0x80` (a D7 glitch) is `(0x01, 0x80)`, which
reads D0 as "min high, max low" and destroys every channel. AND/OR is the per-bit analogue of
min/max — per channel `(0,0)` is low throughout, `(1,1)` high throughout, `(0,1)` **toggled within
the bucket** — so glitches survive decimation exactly as they do for analog.

Decimated encodings are a *view* of a capture, never a capture: nothing decimated may enter the
capture store, which holds full resolution only (§2.7).

A 1 M-sample 2-channel u8 capture is 2 MB raw and ~7 MB as JSON numbers. JSON for sample data is
not viable at these rates.

**Backpressure:** the server tracks per-stream unacked bytes. Above the high-water mark it switches
the stream to its decimated encoding (3, 4 or 5 — one pair per screen column) rather than dropping frames or
buffering unboundedly. A slow client gets a lower-resolution live view, never a stall — and
min/max preserves glitches that plain decimation would alias into a clean-looking lie.

### 3.3 Type sharing

Rust is the source of truth; `ts-rs` emits `web/src/proto/types.ts` at build time. A protocol
change the frontend hasn't adopted becomes a TypeScript compile error, not a runtime surprise.

### 3.4 Recordings and view frames

```jsonc
{ "op": "rec.start", "device": "3-3.4.1.1", "cfg": { "rate_hz": 1000000, "channels": [0,1,2] } }
→ { "rec": 1, "cfg": { ... effective ... }, "calibration": [ ... ] }
{ "op": "rec.stop", "rec": 1 }
{ "op": "rec.list" }
→ { "recordings": [ { "rec": 1, "device": "3-3.4.1.1", "kind": "digital", "rate_hz": 1000000,
      "channels": [0,1,2], "first_sample": 0, "head_sample": 41002100, "bytes": 181520,
      "ratio": 225.9, "state": "recording" } ] }

{ "op": "rec.view", "rec": 1, "follow_s": 0.02, "columns": 1600, "fps": 30 }   // live tail
{ "op": "rec.view", "rec": 1, "t0_s": 3.1, "t1_s": 3.3, "columns": 1600 }      // fixed range
→ { "view": 4 }
{ "op": "rec.view.update", "view": 4, "t0_s": 3.15, "t1_s": 3.25, "columns": 1600 }
{ "op": "rec.view.close", "view": 4 }

{ "event": "rec.state", "rec": 1, "state": "recording" | "stopped" | "error", "msg": "..." }
```

`rec.start` is `acq.configure` plus a continuous start in one step; its result is the effective
configuration, exactly as `acq.configure` reports it (§2.3.1). Views belong to the connection that
opened them and die with it; recordings do not (§2.8).

A **follow** view re-sends the trailing `follow_s` seconds at up to `fps` (default 30, max 60).
Frames are **conflated, never queued**: if the client has not drained the previous frame, the next
tick is skipped. A slow client sees a lower frame rate, never growing latency — the right failure
for a live view, and the opposite of §3.2's capture streams, where every chunk matters.

View frames are binary, with their own magic so the client dispatches on the first byte:

```
off  size  field
 0    1    magic 0xA6
 1    1    version (1)
 2    2    view_id          u16
 4    4    sequence         u32    per view
 8    1    kind             u8     see below
 9    1    channel_count    u8
10    1    sample_bits      u8     8 or 16 (analog); 8 (digital)
11    1    flags            u8     bit 0: follow view; bit 1: range truncated by eviction
12    4    item_count       u32
16    8    first_sample     u64    absolute sample index of the first item
24    8    samples_per_item u64    1 for runs and raw samples
32   ..    payload
```

| Kind | Payload |
|---|---|
| 1 digital runs | `item_count` × (`value u8`, `length LEB128`) — exact edges |
| 2 digital summary | `item_count` × (`AND u8`, `OR u8`) |
| 3 analog raw | raw codes, channels interleaved, `sample_bits` wide, little-endian |
| 4 analog summary | channel-major; per channel `item_count` × (`min`, `max`), `sample_bits` wide |

The server picks the representation: digital sends runs when the window holds at most
4 × `columns` runs, else a summary of at most `columns` items; analog sends raw samples when the
window holds at most 2 × `columns` samples, else min/max. Item boundaries are aligned to absolute
multiples of `samples_per_item`, so a scrolling follow view does not shimmer as the window moves.

Unlike the 24-byte sample frame, a view frame is placeable on its own: `first_sample`,
`samples_per_item` and the recording's `rate_hz` (from `rec.list`) give every item an exact time
span. This closes the known gap that decimated stream frames carry no span of their own.

### 3.5 Terminal frames

Terminal output (§11.2) travels server → client as binary frames — UART data is bytes, not text:

```
off  size  field
 0    1    magic 0xA7
 1    1    version (1)
 2    2    session          u16
 4    4    sequence         u32    per session; detects drops
 8   ..    payload bytes
```

Client → server input is JSON (`term.write`), since keystrokes are small. Terminal frames go
through the connection's bounded queue with the same drop-and-count policy as sample frames, and
a dropped frame shows in the terminal as a visible `[… n bytes dropped …]` marker rather than
silently missing text.

---

## 4. USB module — topology tree and port power

Implements **N-1** (show the device tree) and **Q-04** (native Rust, no `uhubctl` binary).

### 4.1 Topology

Enumerated via `nusb`: hubs, ports, attached devices, speed, VID:PID, product string, power state,
switchability, and whether the device is a known instrument. This is the model the UI tree renders
directly.

### 4.2 Port power control

`usb.cycle` does the whole reset in one call — off, `off_ms` (default 1000), on, then wait up to
`wait_s` for something to enumerate on the port again — and answers what came back and how long it
took. Two `usb.power` calls do the same thing only while nothing fails in between; a failure there
leaves the port off, which is how a bench loses its device under test.

Standard USB hub class requests on endpoint 0 — no interface claim, therefore **no kernel driver
detach**, which is why this coexists with the kernel `hub` driver:

```
Hub descriptor : bmRequestType 0xA0, bRequest 0x06 (GET_DESCRIPTOR),
                 wValue 0x2900 (USB2) / 0x2A00 (USB3)
                 → wHubCharacteristics bits[1:0] == 0b01 means per-port switching (ppps)
Port off       : bmRequestType 0x23, bRequest 0x01 (CLEAR_FEATURE), wValue 8, wIndex <port>
Port on        : bmRequestType 0x23, bRequest 0x03 (SET_FEATURE),   wValue 8, wIndex <port>
Port status    : bmRequestType 0xA3, bRequest 0x00 (GET_STATUS),                wIndex <port>
```

**On Linux the primary mechanism is the kernel's per-port `disable` attribute**
(`…/<hub>:1.0/<hub>-port<N>/disable`, Linux ≥ 6.0), with the raw transfers above only as a fallback.
This was found on the bench, not assumed: a raw `CLEAR_FEATURE(PORT_POWER)` flips the hub's port
power bit (`0000 off`) but the kernel never processes the disconnect, so the powered-off oscilloscope
stayed enumerated indefinitely. `uhubctl --nosysfs` reproduces that exactly; plain `uhubctl`, which
uses the attribute, does not. Via sysfs the device leaves the bus in ~20 ms and returns ~0.5 s
after power-on. The attribute is `root:root 0644`, so `udev/99-awic.rules` grants it separately.

Both hubs on this host advertise `ppps`, verified. Where a hub does not, the UI marks the port
non-switchable rather than offering a control that silently does nothing.

The cost of Q-04 is acknowledged: `uhubctl`'s accumulated per-hub quirk handling is not inherited
and will be rediscovered as needed.

### 4.3 Hot-plug detection

Primary: udev netlink monitor, sub-100 ms.

**The namespace trap:** kernel uevents arrive on a netlink socket **scoped by network namespace**.
A container in its own netns receives *nothing* — the socket opens, binds, and never fires. Hence
`--net=host` in §1.3.

Fallback (Q-07: support both): poll `/sys/bus/usb/devices` at 1 Hz; detection latency rises to
~1 s, nothing else changes. One `DeviceWatcher` trait, two implementations, selected by config.

### 4.4 Switchable ports — discovered, with derived protection

No configuration. Hub ports are discovered at start and on every topology change; the bus is the
source of truth, not a hand-written list. (An earlier design required `[[usb.switchable]]`
entries in the config file; that is gone.)

**Switchable** is a hardware fact: every downstream port of a hub whose descriptor advertises
per-port power switching (`ppps`). Root-hub ports are not switched — they carry the host's own
devices (e.g. the Bluetooth adapter at `3-10` on this bench).

Ports also carry a **label** (`usb.label`): free text for what the port feeds, plus a `dut` mark
for the device under test. Bench wiring is the one thing the tree cannot discover, and the label is
meant to be read before switching a port — `usb.tree` reports it, the UI shows it on the row, and
`usb.cycle` echoes it back with what re-enumerated. Session state, persisted beside the locks.

**Protected** ports stay visible but refuse `usb.power` until unlocked. Protection is *derived
from what is attached*, so a newly connected keyboard is protected the moment it appears:

| Attached | Protected by default | Why |
|---|---|---|
| a hub | yes | switching it cuts every device behind it |
| HID (class 0x03) | yes | keyboard, mouse |
| mass storage (0x08) | yes | may be a disk in use |
| wireless controller (0xE0) | yes | e.g. Bluetooth |
| anything else, or nothing | no | instruments, programmers, UARTs, empty ports |

The user can lock or unlock any port — protect a programmer, or deliberately allow cutting a hub.
Those choices are **session state**, persisted in `usb-ports.json` under `[server] state_dir`
(default `$XDG_STATE_HOME/awic`, else `~/.local/state/awic`; `/var/lib/awic` in the container
image, the host's `state/` via `tools/run.sh`), keyed by hub bus path and port — not
configuration, and never edited by hand.

Enforcement lives in **core**, one check for every transport (web, MCP, SCPI), and it is evaluated
against a *fresh* read of what is on the port at the moment of switching — never a cached tree.

The udev rules grant access to hubs **by class** (`bDeviceClass 09`) rather than by VID:PID, so a
newly connected hub works without editing them.

### 4.5 Re-enumeration contract

Powering a port back on is **not** "the device returns":

1. Port powers on → enumerates unprogrammed (`04b4:602a`).
2. Core detects the add, runs `prepare()`, uploads firmware (§2.4).
3. Device **disconnects and re-enumerates** programmed (`1d50:608e`).
4. Only now is it usable.

The UI shows one `flashing` → `ready` transition, not a device appearing, vanishing and
reappearing. Settle time 2–4 s. In-flight captures are cancelled with a stated cause.

---

### 4.6 Instruments follow the bus

The hot-plug watcher's `usb.changed` events drive the instrument registry, not just the tree:

- **Removed** — the cached instrument and its configuration are dropped and `device.removed` is
  published. A handle to a vanished device is never reused: after a power cycle its FX2 firmware
  is gone.
- **Added** — if a backend claims the device, it is opened and prepared, announced as one
  `flashing` → `ready` (or `error`) transition. Devices no backend claims (hubs, peripherals) are
  ignored.
- **Events while `flashing` are ignored.** An FX2 device disconnects and re-enumerates *during*
  its own `prepare()` — that is the firmware upload. Acting on those events would discard it
  mid-flash. This only works if events are handled concurrently with preparation, so preparing
  runs on its own task.

Verified on the bench by power-cycling hub `3-3` port 3 over the WebSocket: the logic analyzer left the
registry, returned as `flashing`, its upload's own `removed`/`added` were ignored, it reached
`ready`, and a capture on the returned device succeeded.

### 4.7 Acquisition end

The end of a stream is the single place an acquisition's end is announced, as `acq.state`:
`stopped` with `complete`; `stopped` with "stopped by request after N samples"; or **`error`**
with "ended early: N of M samples" when the device went quiet before delivering what was
configured (e.g. a stall the driver detected). `acq.stop` stops the instrument and lets the
stream drain and file its partial capture — it never aborts the stream, which would discard a
capture the client already holds a `capture://` handle for.

**A start that delivers nothing is started again, not reported.** At the top of the fx2lafw rate
ladder (24 MHz, where 8 bits × the rate leaves the 4 KB FIFO no margin) roughly one back-to-back
capture in ten to twenty comes back at *zero* samples while its neighbours deliver every sample
with no dropped transfers (live test B-26). The device is not wedged — the next start streams
perfectly — and priming first does not prevent it, because the priming acquisition streams too.
Since nothing has reached the sink at that point, there is no partial timeline and no timestamp
to shift: the driver re-sends the start command and resubmits its transfers, up to twice, and
only then ends the acquisition with `error`. The caller sees a capture one stall timeout slower
rather than a failure it could only answer by retrying itself, and
`AcquisitionStats::silent_restarts` counts it. A stall **after** data is a different state — the
degraded device of §4.7's B-25 note — and still ends the acquisition, so a partial capture is
never silently stitched to a second attempt.

## 5. Graph module

### 5.1 The oscilloscope's mode is a physical switch — and it collides

The oscilloscope selects scope or logic mode with a **hardware switch**, and the switch **changes the
USB product ID**:

| Switch position | Enumerates as | Claimed as |
|---|---|---|
| Scope | `04b4:602a` → `1d50:608e` after firmware | 2-channel oscilloscope |
| Logic | **`0925:3881`** | 8-channel logic analyzer |

`0925:3881` is **the same VID:PID as the standalone fx2lafw logic analyzer**, and neither device reports a
serial number. This is what produced the phantom "second logic analyzer" early in the project.

Consequences:

- There is no software mode control, so no `set_mode()` (§2.3). M4 is smaller than first specified.
- **VID:PID is not a unique key on this bench.** Devices are identified by **bus path**, which is
  already the `DeviceId` key. The UI must label by location, not by model, or the two `0925:3881`
  devices become indistinguishable to the user.
- Per R-03, the oscilloscope is presented as a scope. Found at `0925:3881`, the UI says so plainly —
  *"oscilloscope in logic mode, or an fx2lafw logic analyzer; flip the switch for scope"* — rather than silently
  treating it as a generic LA.

### 5.2 Capability-driven UI

The device panel is generated from `capabilities()`, not hardcoded per model: `Analog` yields
channel/scale/offset controls, `Digital` yields channel/threshold controls. A new driver gets a
working UI for free.

### 5.3 Renderer abstraction (Q-06, R-06)

Switchable at runtime behind one interface:

```ts
interface WaveformRenderer {
  readonly id: 'plotly' | 'webgl2' | 'uplot';
  mount(el: HTMLElement, layout: Layout): void;
  setTraces(traces: TraceSet): void;
  setViewport(t0: number, t1: number): void;
  /** User zoom/pan; live mode turns it into rec.view.update (§5.5). */
  onViewportChange(cb: (t0: number, t1: number) => void): () => void;
  setTheme(colors: ThemeColors): void;
  destroy(): void;
}
```

**WebGL2 is the graph default** (D-05, revising R-06): Plotly is built for figures, not streams,
and live mode (§5.5) redraws at up to 60 fps. Plotly stays selectable and remains the right choice
for PSU logging plots. It is still loaded by dynamic import, so it costs nothing until selected.

| Backend | Strength |
|---|---|
| `webgl2` | streaming; 16 digital + 4 analog lanes at 60 fps; **default** |
| `plotly` | richest interaction out of the box; ~1 MB gz |
| `uplot` | tiny, excellent to ~100 k points (not implemented) |

The WebGL2 renderer, `render/webgl2.ts`, no dependencies:

- **Lanes**: analog panels drawn as line strips (raw) or filled min/max bands (envelope);
  digital lanes as level lines with vertical edges, and `toggled` segments as a filled band —
  never as a level (§3.2).
- **Axes, grid and labels** on a 2D canvas overlaid on the GL canvas: time in SI units
  (ns/µs/ms/s), volts per analog panel, channel names per lane.
- **Interaction**: wheel zooms time about the cursor, drag pans, double-click fits; each change is
  reported through `onViewportChange`. Cursors are vertical rules the graph module owns.
- Device-pixel-ratio aware; sized by a `ResizeObserver` on a container whose height is definite
  (the Plotly growth bug, §11.3) — the renderer never derives its container's height from content.
- Draws only on change (new traces, viewport, resize, theme), coalesced to one frame per
  `requestAnimationFrame`; idle, it costs nothing.
- Colors from the theme tokens (§11.1), one hue per channel, consistent across renderers.

### 5.5 Live mode

The graph runs in one of two modes, chosen in the toolbar:

| Mode | Source | Behaviour |
|---|---|---|
| **Single** | `acq.*`, capture store | a finite, triggered capture; as before |
| **Live** | `rec.*`, recorder (§2.8) | a follow view of the running recording at up to 30 fps |

In Live mode, zooming or panning **pauses following** without pausing the recording: the view
switches to a fixed range served from history (`rec.view.update`), and a *Follow* button resumes
the tail. The toolbar shows the retained span and the memory in use, so a user can see how far
back they can scroll. *Snapshot* turns the visible range into a capture (§2.8.5) for measurements.
A pattern search box (§2.9) lists matches as clickable timestamps that jump the view.

### 5.4 Measurements

Computed **server-side in Rust on full-resolution data**, never in the browser on decimated data —
otherwise measurements silently degrade as the user zooms out, which is the worst kind of wrong:
plausible and unflagged.

Analog: Vpp, Vmin, Vmax, Vavg, Vrms, frequency, period, duty, rise/fall.
Digital: frequency, period, duty, pulse width, edge count.

**Nothing analog is timed inside the noise floor.** Edge detection is hysteresis-based, but the
dead band scales with the waveform's own span, so on a flat input it shrinks with the noise and
the converter's dither keeps qualifying as edges — an open oscilloscope input spanning one ADC code
reported 74.9 kHz at 16.6 % duty (live test B-04). Frequency, period, duty and rise/fall are
therefore reported only when the channel's peak-to-peak span reaches **4 ADC codes** of its own
range; under that they come back `unavailable` naming the span in codes. Amplitude
(Vpp/Vmin/Vmax/Vavg/Vrms) is never gated — those are as meaningful on a noisy line as anywhere
else — and a genuinely small signal is timed as soon as it clears the floor (a ten-code square
wave measures its 1 kHz exactly). The eye view (§5.6) folds on the same timing and refuses a
channel under the same floor: on a probe at no signal it once "recovered" an 889 kHz clock from
four codes of noise and called it `data` (B-105).

**NRZ data is no clock (B-103).** When the runs between a line's edges are whole numbers of one
short run — at least three different lengths, at least 10 % of the runs at one unit and 10 % at two
or more, runs of ≤ 2 samples and over 16 units left out — the line carries bits (UART, CAN), and
its frequency, period and duty are averages over runs of different lengths. They keep their value
and carry a note that says so and gives the unit interval and bit rate, which do mean something.

### 5.6 Rendered views (M16)

A capture drawn **server-side**, returned as an SVG document together with the numbers the
drawing measured. `capture.render` on the wire, `render_view` over MCP, `GET /api/render` for the
browser, and the same renderer behind all three — so the picture a person shares from the web
UI's **Image** button and the picture an agent receives are the same picture, measured the same
way.

This does not replace the browser's WebGL traces (§5.3), which stay live and interactive. It
exists for the callers that have no screen: an agent asking "what does this look like?", a script
saving a view, a person pasting one into an issue.

**The numbers are the primary channel, not a caption.** Every view returns `metrics` — each with
a `name`, a `value` in its base unit for arithmetic, a `unit`, and the `text` the picture is
labelled with — plus `findings`, the things it found, each located in time where it has a place;
and it repeats the whole reading in the SVG's `<desc>`. A model reading `eye_height = 3.13 V` is
not inferring it from pixels, and a caller with images switched off still gets everything. The picture's job is the part a list of numbers cannot carry: *where* the
jitter is, *which* edge is slow, whether there are two populations.

**Every view has a `url`** over MCP (F-09): the same request as `GET /api/render` — capture,
kind, channels, size, window, format — so an agent saves the picture into its report with
`curl -o`. It is absolute (`http://<host>/api/render?…`) when the MCP client's `initialize`
named the server's `Host` (the stdio shim does), else the path alone; it never carries the bearer
token.

**SVG first.** The annotations stay text in the document, so they can be parsed rather than
recognised, and it scales in a browser without blurring. A client that cannot display SVG asks
for `format: "png"` (`render_view`; `format=png` on `GET /api/render`): the same document
rasterised by resvg, on a white page at one pixel per unit (F-09 — Claude Code saved the SVG image
block to a file and showed nothing). The executable embeds no font: labels are drawn with the
host's installed fonts (the container installs `fonts-dejavu-core`), and on a host with none the
PNG comes without labels and `notes` says so.

#### 5.6.1 Waveform

Amplitude against time, one lane per channel. Every column is drawn as the **min/max band** of
the samples behind it, never as one sampled point: at 24 MS/s across 900 pixels a column stands
for thousands of samples, and picking one of them hides exactly the short glitch the view is
being consulted about. When that happens the result says so in `notes` — a reader who does not
know a column stands for a thousand samples will read every wiggle as a real edge.

#### 5.6.2 Eye

Every period of **one** channel folded onto the same axes, two unit intervals wide, and shaded by
how often traces land in each cell — logarithmically, because the settled levels are hundreds of
times denser than the edges the view exists to show. It turns thousands of edges into one picture
of how repeatable a signal is: jitter, overshoot, a slow edge and two populations all become
visible where a single trace shows none of them.

The unit interval is recovered from the median interval between rising mid-level crossings unless
the caller gives one, and which of those happened is reported — a recovered period that is wrong
makes every other number wrong with it.

**Two genuinely different pictures come out of the same fold, and the result says which.** A
*clock* folded on its own period lands every trace on top of every other, so no phase carries
both levels and a textbook eye measurement measures nothing; what matters there is how long the
signal stays settled and how much room the two levels leave a threshold (`kind: persistence`).
*Random data* is the opposite: every phase carries both symbols and the opening between them is
the whole point (`kind: data`). Reporting one `eye_width` without saying which case produced it
would be reporting a different quantity depending on the input.

A third case has no two levels at all: a sine, a triangle, a heavily filtered square. Its
crossings jitter by a sample, so the columns at its zero crossings carry both "levels" with a
sliver between them, and the fold used to call a 101 kHz sine `data` with an eye height of
13.6 mV (B-74). The two levels are taken as the medians of the samples above and below the
threshold — what a square spends its time at, which overshoot and spikes do not move — and a
signal that holds fewer than half its samples within a tenth of the level difference of its own
level (a square or NRZ line holds over 90 %, a sine 26 %, a triangle 20 %) folds as
**`kind: periodic`**: the picture is the persistence shading alone, with no
threshold line or opening marker, and `eye_height`, `eye_width`, the levels and the settled
stretches are left out — only `unit_interval`, `min`/`max` and the jitter numbers mean anything
there.

Two jitter numbers are reported, because they are not the same thing and quoting either alone is
how they get confused:

- `jitter_tie_pp` — time interval error, peak to peak: how far the crossings stray from a
  **least-squares fit** of crossing time against crossing index. This is what closes an eye
  sideways. The fit matters: an earlier version fitted the grid with the median period, and a
  perfectly steady 40-cycle signal came back with a whole period of "jitter" — accumulated drift
  from a period that was off by a few percent.
- `jitter_cycle_to_cycle_pp` — the largest change from one period to the next. A signal can have
  small cycle-to-cycle jitter and large TIE (slow wander), or the reverse.

The view **refuses rather than drawing nothing**, naming what to do differently: a constant
channel has no edges to fold, a period under 8 samples cannot be folded at a useful resolution,
fewer than three whole periods is not a diagram, and more than one channel is not an eye.

#### 5.6.3 Histogram

How long a channel's intervals are, as a distribution on a **log axis**, with the populations they
fall into named. Periods by default (`interval_kind`), or high/low pulse widths.

The log axis is not a preference: a capture holds 80 ns glitches next to 1 ms pulses, and a linear
axis puts every one of them in the leftmost pixel.

**Populations, not bins.** Intervals are grouped by splitting the sorted lengths wherever the step
to the next exceeds **20 %** of the group so far. A fixed bin width cannot work for this, because
it either merges 4.34 µs with 8.68 µs or splits 1 ms into forty bins depending on which end of the
capture it was tuned for; the question a reader is actually asking is "is this the same *kind* of
pulse". Populations come back biggest-share first — the first one is what the signal normally does
— and are capped at 8, because past that the honest answer is "a continuum", and a list of forty is
no more readable than the samples were.

This is the fastest view to diagnose a digital line, and the reason is compression:
`population_1_length 8.68 µs (97 %)` beside `population_2_length 4.34 µs (3 %)` is a complete
finding — a line running at half its expected rate — in two numbers, where a picture of four
thousand edges is not. Each population also carries its **spread** (min to max), which is the
jitter *within* one kind of pulse that a mean hides.

Intervals of **2 samples or fewer are counted as glitches** and noted: two samples is the shortest
thing a sampler can represent at all, so anything at that width says more about the sampling — or
about a floating input — than about the signal, and the note says to capture faster to tell them
apart.

Only whole runs are measured. The first and last runs in a window are cut short by the window
rather than by an edge, and counting them invents a short pulse at each end.

#### 5.6.4 Anomaly digest

The same intervals, but only the ones that do **not** fit: how many are outside a tolerance
(default 5 %), the worst deviation, and `findings` naming **where** the worst ones are, each with
an `at_s`.

That last part is the point. A caller hands a timestamp straight back as `t0_s`/`t1_s` of a
waveform call and looks at what happened, instead of paging through seconds of samples. `findings`
exists on every view for this reason, and is what makes a 48-million-sample capture something an
agent can act on rather than merely summarise.

The reference is the **dominant population's** mean, never the overall mean — an overall mean is
dragged by the very outliers being looked for, so ninety good periods and ten long ones would make
all hundred of them "9 % off". Outliers come worst-first and are capped at 12 named ones, with the
full count reported alongside; a list of four thousand timestamps is the samples again in a
different shape.

**On NRZ data the reference is the unit interval (B-102).** A CAN or UART line's runs are 1, 2, 3…
bit times; held against the dominant population, every run of more than one bit was a "+100 %" to
"+400 %" outlier. When the populations sit at whole multiples of the shortest (the same rule as
§5.4's, with populations instead of runs: three multiples, 10 % at one unit, 10 % at two or more,
under 10 % fitting none), each interval is held against the nearest whole number of units and its
deviation is a fraction of one unit; `reference_kind` says which reference was used. Runs over 16
units are idle gaps between frames or bytes and are not judged (`idle_gaps`). A clock that drops a
pulse now and then has a few periods at 2× — two multiples, a small share — and those stay
anomalies. When the tolerance is finer than one sample, the `notes` say so: an edge one sample off
is then an outlier by construction.

The picture is **deviation against time**, with the tolerance as a band. That shape says which
*kind* of problem it is — one spike is a stutter, a slope is a clock drifting, a wide band is
jitter — and the count alone distinguishes none of them. When nothing is outside the tolerance the
result says so explicitly, because an empty findings list that does not explain itself reads like
a failure.

---

## 6. Protocol decoders (Q-14)

Native Rust. v1 scope: **I²C, SPI, UART**; **CAN** (classic 2.0A/B, §6.2) and **1-Wire** (§6.4)
followed.

```rust
pub trait Decoder: Send {
    fn name(&self) -> &str;
    fn required_channels(&self) -> &[ChannelRole];   // SCL, SDA, ...
    fn options(&self) -> &[DecoderOption];           // baud, bit order, CPOL/CPHA, address width, CAN bitrate
    /// Streaming: feed transitions in order, plus the sample index through which the input is
    /// known (no transition up to `valid_until` means the level held). Annotations come out as
    /// soon as they are complete. The rate is fixed at construction.
    fn feed(&mut self, transitions: &[Transition], valid_until: u64) -> Vec<Annotation>;
    /// The lines' real levels at the first sample, before the first `feed`: input may start
    /// mid-transaction (a decode window, a capture, a live tail), and a decoder that assumed its
    /// bus idle there would read the first sample as an edge — for I²C a phantom START and
    /// shifted bytes. Seeded, it waits for the next genuine frame boundary (I²C START, SPI CS).
    /// UART ignores it: a low first sample is most likely a start bit.
    fn seed(&mut self, levels: u8);
    /// End of input: flush a frame that is still open, marked incomplete.
    fn finish(&mut self, end_sample: u64) -> Vec<Annotation>;
}

pub struct Annotation {
    pub start_sample: u64, pub end_sample: u64,
    pub level: u8,                    // 0 frame, 1 field, 2 bit
    pub kind: AnnotationKind,         // Data(byte) | Start | Stop | Ack | Nack | Address | Error ...
    pub text: String,
}
```

Decoders consume the **transition list** (§2.7) — `(sample_index, packed byte)` — not raw samples:
far cheaper, and it keeps the decode layer independent of how samples were acquired. A recording's
runs (§2.8.1) are the same data, so one streaming decoder serves both a finished capture and a
live recording. Crate **`awic-decode`**, pure, no dependencies beyond `std`.

Positions are sample indices, not nanoseconds: exact, and the caller knows the rate. Annotations
nest by `level` (frame → field → bit) so the UI can stack them under the trace, and are available
over all transports and in exports. A live UART decode also feeds the terminal (§11.2).

Operations: `decode.list` (decoders, their channel roles and typed options, so a UI or agent builds
its form without hardcoding; an option it does not list is refused with the ones it does, where it
used to be dropped and the decode run on the default — `baudrate` for `baud` decoded at 115200,
B-106), `capture.decode` (optionally over `[t0_s, t1_s)` of the capture,
seconds from its start as in `read_capture` — B-101: the window used to be accepted and ignored)
and `rec.decode` (a recording range, streamed in 1 M-sample windows). Either window is seeded with
the lines' levels at its start, so one opened mid-frame waits for the next frame start. Results are bounded like every agent-facing output (§8.3): 500 annotations by
default, 5000 at most, with the number omitted. MCP: `decode`, `list_decoders`; SCPI: `DEC:LIST?`,
`DEC:CAPT?` (e.g. `DEC:CAPT? "CAN",(@1)`, channels bound in the decoder's role order; SCPI passes
no options, so every decoder runs with its defaults — CAN at 500 kbit/s, `dominant_low` — and no
`minus`).

**Output shapes.** `format: "annotations"` (default) returns the nested units the graph draws;
`max_level` drops what is nested deeper than asked (0 frames, 1 fields, 2 bits) *before* the bound
applies, because bit-level annotations are roughly three quarters of a decode and a caller reading
bytes never wants them — MCP therefore defaults to level 1. `format: "transactions"` returns one
entry per I²C START..STOP, UART burst (split on an idle gap of more than two character times),
SPI chip-select window or CAN frame, with the bytes as hex, a UART burst's text, and notes for
NACKs, framing errors and an incomplete end. One 1 KB display write is ~11 000 annotations but 66 transactions.

- **Bounded bursts (B-97).** A UART burst, or an SPI burst without CS, holds at most `max_bytes`
  data bytes (default 256, at most 4096) and continues in the next entry, noted "continues the
  previous transaction": a 1999-byte burst came back as one ~10 KB entry, its bytes once as hex
  and once as text. Framed transactions — a START..STOP, a CS window, a CAN frame — are never cut.
- **Repeats folded (F-14).** Consecutive transactions identical in everything but their timing
  (address, direction, data, MISO, acknowledgement, errors, notes) come back as one entry with
  `repeat: n`, `start_sample`/`t_s` of the first and `end_sample` of the last: 164 retransmissions
  of one unacknowledged CAN frame were 164 objects. `fold: false` lists each. The `summary` still
  counts every transaction (`transactions`, NACKs, errors, `by_address`), and `folded` says how
  many went into an earlier entry. The cap counts returned entries.
- **Bursts (F-14).** `summary.bursts` describes the burst structure when there is one: the gaps
  between consecutive transactions are sorted and split at their largest jump, each counted with
  the median transaction length added so back-to-back transfers do not read as a jump; a jump of
  less than 5× means evenly spaced transactions and no `bursts`. It gives `count`,
  `transactions_min`/`_max`, the median `period_s` (start to start) and `duration_s`, and the
  `gap_s` that separates bursts — the MCP2515 fixture's RX STATUS poll comes out at its 54 µs.
- **A missed CS release (issue #32).** A CS window whose clock pauses for more than 1.5 words, and
  more than 3× the window's typical gap between words, carries a note naming the word and sample:
  two transfers may share it because CS went high for less than a sample (at 16 MS/s a LOAD TX
  and its RTS decoded as one 15-byte transaction). A hint, not an error.

**Auto-baud.** UART's `baud` accepts `"auto"` (or `0`): the rate is estimated from the line's own
pulse widths — every pulse is a whole number of bit times — and reported as `detected_baud`. It
needs a few characters of traffic and refuses rather than guesses when the widths are no integer
multiple of a common bit time (`baud_not_detected`).

### 6.2 CAN

Classic CAN 2.0A/B (`DecoderId::Can`, `awic-decode/src/can.rs`), read from **one line**: the role
`can` is the RX pin of a transceiver, CANL or CANH, or a differential pair thresholded to one
level (§6.3). Frames are found by timing alone: a recessive→dominant edge after at least ten
recessive bits is a start of frame and hard-synchronises the bit clock, every later
recessive→dominant edge resynchronises it, so a transmitter a percent or two off the configured
rate still decodes over a 130-bit frame. Bits are sampled at the sample point, destuffed and parsed
into fields (11- or 29-bit identifier, RTR, IDE, DLC 0–8, data, CRC-15, ACK, EOF), and the CRC is
checked. After a frame, or after anything that is not one (a stuff error, a form error, an error
flag), the decoder idles until the bus has been recessive again, which skips error flags and
delimiters and the tail of a frame a decode window opened in the middle of. Idle is counted from
where the input begins (`Decoder::seed_at`: a window opened deep into a recording has seen none
yet), and activity skipped before the first idle is reported as one `incomplete` frame-level
annotation (`bus activity before the first idle …`), never left blank. A frame cut by the end of
the input inside its EOF is still reported as a frame: everything it carries, ACK included, was
read. A CAN FD frame (FDF recessive) is named as such (`CAN FD frame (FDF set): not decoded`)
instead of being misread as a CRC error; FD itself is not decoded.

Options (`decode.list` has the types and defaults):

| Option | Default | |
|---|---|---|
| `bitrate` | 500000 | bit/s, 1–1 000 000. `"auto"` or `0` estimates it from the line's shortest pulses (a stuffed bus makes every pulse one to six bit times), snaps it to a standard rate (10, 20, 50, 83.333, 100, 125, 250, 500, 800 kbit/s, 1 Mbit/s) when within 3 %, and reports it as `detected_baud`; `baud_not_detected` when the widths fit no bit time |
| `sample_point` | 75 | percent of the bit time, 50–90 |
| `polarity` | `dominant_low` | `dominant_low` for the RX pin or CANL; `dominant_high` for CANH or a thresholded CANH−CANL |

Fewer than four samples per bit is refused. Annotations on the usual three levels: bit (stuff
bits), field (SOF, ID, RTR, IDE, DLC, data, CRC, ACK, EOF) and frame (`0x123 [8] DE AD BE EF 01 02
03 04`, plus `no ACK`, `CRC error`, `stuff error`); error and overload frames are reported as such.
`format: "transactions"` gives one entry per frame: `address` is the identifier (the wire type is
`u32`, a 29-bit identifier does not fit a `u16`), `data`, `count`, `acked` (false for a frame nobody
acknowledged — on a bus with a single node that is every frame, and it retransmits), and `notes`
(`extended`, `remote`, `no ACK`, `CRC error`, `error frame`). The result's `summary` counts per
identifier, with `nacks` meaning unacknowledged frames. The live decode terminal (§11.2) shows one
candump-style line per frame: `  can  123   [8]  DE AD BE EF 01 02 03 04`, with `NO-ACK`,
`CRC-ERR`, `remote` and `ERROR FRAME` where they apply (an extended identifier prints eight hex
digits).

### 6.3 Decoding analog captures

A decoder consumes logic levels, so an oscilloscope `capture` is first cut into transitions
(`awic-core/src/threshold.rs`; GitHub issue #7) and then decoded exactly like a digital one — any
decoder, not only CAN. Each role's source is one hardware channel, or, with `ChannelMapping.minus`,
the difference `channel − minus` (a differential pair: CANH as `channel`, CANL as `minus`, with the
polarity `dominant_high`). A source is cut with a Schmitt trigger, the same dead-band idea as the
measurement edge detector: a sample flips the state only once it clears the far side of a band
around the threshold, and the edge is stamped where the signal last crossed the threshold itself,
so a slow edge is not delayed by half its rise time.

Unless overridden the threshold is automatic and per source: halfway between the line's two
dominant levels, found from a histogram of the capture (a spike, ringing or a long idle stretch
does not move it). The default dead band is a fifth of the swing, never less than 1.5 ADC codes
(an 8-bit scope quantises a small swing so coarsely that anything narrower chatters on one code of
noise). Options `threshold_mv` and `hysteresis_mv` (integer millivolts; hysteresis is the total
width of the dead band, not negative) override them for every role; they, and `minus`, are refused
on a digital capture (`invalid_decoder_config`). A source that never leaves one level — a channel
not on the bus — has no threshold to find and reads as constant, high above about 1.4 V and low
otherwise. The result's `note` says what was used, e.g. `analog: CH1 threshold 0.912 V,
hysteresis 0.036 V; CH2 flat at 0.002 V (read as constant low)`, so a wrong guess is visible.
For CAN on a difference (`minus`) the automatic threshold is held to the standard one, ISO
11898-2's 0.7 V between recessive (< 0.5 V) and dominant (> 0.9 V), mirrored for CANL − CANH,
with the dead band kept inside that window (B-99): the midpoint of the two levels sat above a
weakly driven node's ACK slot and reported every acknowledged frame unacknowledged. A smaller swing
— 10:1 probes the scope is not told about — keeps its own, lower midpoint, and `threshold_mv`
always wins. When one side of a difference never moves, the `note` names it: the difference is
then the other channel alone, usually a probe off its wire.
`rec.decode` stays digital-only: a recording has no voltage to threshold (snapshot an analog
recording first).

### 6.1 Survey (which pin does what)

`survey` answers what is on a digital capture or recording range without knowing the protocols in
advance: per channel its resting level, edges, shortest high/low pulse, glitch count (spikes of ≤ 2
samples: a floating input or crosstalk, never decoded), burst rhythm and a one-line summary; plus
guesses — a UART line with its baud rate, an I²C clock/data pair with its bit rate, an SPI bus, a
free-running clock with its frequency and duty. I²C is recognized by where the data line's edges fall
relative to the clock's level: every data edge while the clock is high is by definition a START or a
STOP, and on a real bus those are few and paired, which also tells SCL from SDA.

SPI is recognized the same way, from the other side: a data line changes at most once per clock
pulse, at (or just after) the shift edge, so inside a clock burst — within two clock pulses of a
clock edge — all its edges fall while the clock is at one level, where an unrelated line's split
between both. Which level gives the sampling edge, the clock's resting level gives CPOL, and the two
give the mode. The clock must pause somewhere — a steady stretch of at least four pulses, as between
transfers — or a ripple counter would pass: each of its bits changes exactly on the lower one's
falling edge, like data on a clock. A chip select is a line that changes at most a quarter as often
as the clock and holds one level for at least 98 % of the clock's edges — the asserted level,
reported as `cs_active_high`. It is taken from the clock, not from where the line rests longest: a
long transfer keeps CS asserted longer than the gaps between transfers. And it brackets the bursts:
at least half its edges come within 16 clock pulses of a clock edge (the releases), which a quiet
line that merely sits still while the clock runs — an idle UART — does not. CS is looked for before
the data lines, because its release follows the last clock edge as closely as a data change does.
The guess is `spi {clk, data, cs, cs_active_high, mode, clock_hz, undersampled}`: `data` lists MOSI and MISO
without saying which is which — both are clocked by the same edges, and only the board knows who
drives them. An I²C pair is taken first, or SDA would read as mode-3 data; in turn a pair is no I²C
when more of its data edges fall on the very sample the clock rises at than it has STARTs and STOPs
— SDA must hold while SCL rises, and SPI mode 2 with a pulled-up MISO, whose data edges land on the
clock's rising edge, otherwise passes for I²C. A line whose every pulse is two samples or fewer is
a spike train — crosstalk or a floating input (`glitches_only`) — unless it toggles back and forth
in runs (at least half its resting stretches as short as its pulses) and has more than twice the
edges of every other line, which no crosstalk has: then it is a signal at the sampling limit,
`undersampled` (B-100: a 5.5 MHz MCP2515 clock at 16 MS/s was called crosstalk, and the bus was
invented from the lines left over). Such a line is still an SPI clock, with its pulse taken from
the short pulses; the guess then says `undersampled: true` and has no `clock_hz`. That finds all
four modes up to 5.5 MHz at 16 MS/s (2.9 samples per period); at 7 MHz the survey may find
nothing, but never a wrong mode, and the decoder still reads every byte.
`tests/spi_virtual_bench.rs` checks all four modes from 100 kHz to 3.2 MHz (and pins the limit up
to 7 MHz) against a simulated
master and slave — skew, jitter, an asymmetric clock, CS of either polarity, long transfers — and
that a wrong sampling edge reads wrong bytes.

Streamed and bounded like `rec.decode`. MCP: `survey_channels`, which will take the capture itself
when given a `device`.

### 6.4 1-Wire

`DecoderId::OneWire` (`awic-decode/src/onewire.rs`), one open-drain line, role `owr`. There is no
clock: the decoder times the master's low pulses. A low of at least 360 µs (three quarters of the
480 µs minimum) is a reset; a low that starts within 120 µs of the reset's release is the presence
pulse a device answers with. Every other low is a time slot whose bit is the line's level 15 µs
after the falling edge — a write-1 or a read-1 is low for less, a write-0 or a slave's read-0
longer — so a read slot needs no knowledge of who drove it. A low between 240 µs (twice the longest
write-0) and the reset length is no slot: reported as an error, and byte boundaries are lost until
the next reset. Overdrive timing is the same at roughly a tenth: bit at 3 µs, slot up to 32 µs,
reset from 36 µs, presence within 16 µs.

On the bits sits the ROM layer every device follows. The first byte after a reset is the ROM
command (Read ROM `33`, Match ROM `55`, Skip ROM `CC`, Search ROM `F0`, Alarm Search `EC`, Resume
`A5`, Overdrive Skip ROM `3C`, Overdrive Match ROM `69`). Read, Match and Overdrive Match ROM are
followed by the 64-bit ROM code; Search ROM and Alarm Search by 64 triplets (the bit, its
complement, the bit the master writes), of which the written bits are the ROM code found — a
triplet whose bit and complement both read 1 means no device answered and is reported as an error.
A ROM code is named the way Linux's w1 names a device, family code and 48-bit serial
(`28-00000A1B2C3D`), and its CRC-8 (x⁸ + x⁵ + x⁴ + 1) is checked. Bytes after that are data, LSB
first. Overdrive Skip and Overdrive Match ROM switch the decoder to overdrive timing; a reset at
standard length switches it back, as it does the devices.

| Option | Default | |
|---|---|---|
| `speed` | `standard` | `standard` or `overdrive`: the speed before the first reset, for a window opened on a bus already in overdrive |

The sample rate must give at least four samples in the bit-sampling time: 267 kS/s at standard
speed, 1.34 MS/s for overdrive. A slower rate is refused at standard speed; a bus that switches to
overdrive below its rate gets an error annotation saying the bits after it are unreliable. Until
the first reset byte boundaries are unknown, so earlier slots — a capture that starts
mid-transaction, or a window opened inside a low pulse (`seed`) — are counted, not decoded, and
reported as one frame-level `incomplete` annotation.

Annotations: frame (`reset`, `reset (overdrive)`, `reset, no presence`), field (the ROM command by
name, `ROM 28-00000A1B2C3D` with `CRC error` when it does not match, data bytes) and bit (each time
slot). On the wire `one_wire_rom` carries `rom` as that string and `crc` separately: the 64-bit
code does not fit a JSON number exactly. `format: "transactions"` gives one entry per reset:
`acked` is whether a presence pulse answered it (`nacks` 1 when none did), `address` the ROM's
family code, `data` the bytes after the ROM layer, `notes` the ROM command and code. The live decode
terminal (§11.2) shows one line per reset: `reset  Match ROM  28-00000A1B2C3D  44`, with
`NO-PRESENCE`, `overdrive` and `CRC-ERR` where they apply. SCPI: `DEC:CAPT? "1-Wire",(@n)`.

---

## 7. PSU module

No PSU attached yet (Q-13). Ship the `PowerSupply` trait plus a **generic SCPI-over-serial**
template driver (R-05), which works with many bench supplies (Korad/RND KA3005P, Tenma 72-xxxx,
several Rigol/Siglent) given per-model tweaks — and shares a command vocabulary with the SCPI
server (§9), so one mental model covers both. A fake PSU backs the tests.

### 7.1 Soft limits (Q-10)

You declined confirmation gates and asked for configurable safe limits instead, which makes those
limits the *only* guardrail — so where they live matters:

```toml
[[psu.limits]]
device = "*"
v_max  = 5.5
i_max  = 1.0
```

Enforced in **core**, on every setpoint, from every transport. A limit implemented in the MCP
layer would leave the SCPI port as an unchecked path to the same hardware. Violations are refused
with a named error, logged, and pushed to the UI as an event.

---

## 8. MCP server (Q-09)

### 8.1 Transport

Streamable HTTP at `POST /mcp` on the same axum server — one port, one process. A stdio shim
(`awic mcp-stdio`, usable via `podman exec`) is provided for clients that need it.

`initialize` returns `instructions` (F-15): the workflow (survey → capture or record → decode →
measure and render), the hardware channel numbering every tool uses (D4 is 4, CH1 is 0), the
windows (`t0_s`/`t1_s`, `span_s`, and `sample0`/`sample1` — the sample numbers `decode` reports,
which `read_capture` and `render_view` take, issue #32) and which fields carry a result's caveats.
Clients load tools lazily, so one tool's description may be all a model sees otherwise.

Every tool call is translated into the same protocol request the web UI sends and runs through the
one dispatch path, with origin `mcp <client>`. That is what makes port protection, soft limits and
mutation logging identical across transports — the reason the JSON-RPC surface is implemented
directly rather than through an SDK's own handler stack. Answers are single JSON bodies; no tool
needs server-initiated messages.

### 8.2 Tools

| Tool | Returns |
|---|---|
| `list_instruments` | devices, capabilities, power state |
| `usb_tree` | hub topology with per-port switchability |
| `set_usb_power` | new state; refuses protected ports |
| `set_usb_port_protected` | lock or unlock a port (session state) |
| `configure_acquisition` | validated effective config |
| `capture` | `capture_id` + **summary stats only** |
| `read_capture` | decimated samples, `max_points`-capped |
| `measure` | scalar + unit |
| `decode` | annotations, or one entry per bus transaction (§6) |
| `survey_channels` | per-channel activity + protocol guesses (§6.1) |
| `power_cycle_usb_port` | off, wait, on, and what enumerated back |
| `label_usb_port` | names what a port feeds; marks the DUT (§4.4) |
| `render_view` | a `waveform`/`eye`/`histogram`/`anomalies` picture plus its `metrics` and `findings` (§5.6) |
| `list_decoders` | the decoders and their roles and options (§6) |
| `start_recording` | `rec` id + effective config; takes `ranges_v`, refuses unknown arguments (§2.8) |
| `stop_recording` / `delete_recording` / `list_recordings` | stopped / freed / every recording with its state and memory |
| `search_pattern` / `wait_for_pattern` | matches in a recording's history / the next match as it happens (§2.9) |
| `snapshot_recording` | `capture_id` of a window, usable with every capture tool |
| `serial_list` / `serial_open` / `serial_write` / `serial_read` / `serial_expect` / `serial_close` | serial ports as terminal sessions (§11.2) |
| `psu_set` / `psu_read` | *planned (M6)*: applied setpoint / V,I,P |
| `export_capture` | *planned (M9)*: resource URI |

27 tools today; `tools/list` is the authoritative catalog, with units, limits and an example in
every description.

### 8.3 The context-budget constraint

A hard design rule: **no tool returns bulk sample data.**

`capture` returns a handle plus derived scalars. "What's the frequency on channel 0" should cost an
agent a few hundred tokens, not two megabytes. `read_capture` caps `max_points` server-side
(default 512, hard max 4096) and min/max-decimates above that.

Full-resolution data leaves only through `export_capture`, as a resource URI.

### 8.4 Agent-facing safety

Port protection (§4.4) and soft limits (§7.1) live in core, so MCP inherits them with no bypass path.
Every MCP mutation is logged with arguments and origin and pushed to the web UI as a live event —
a human at the bench can see what the agent just did.

### 8.5 MCP monitor — the web UI's MCP tab

The MCP tab answers three questions for the person at the bench: *how do I connect an agent*,
*who is connected*, and *what did it just ask and get back*.

**Connection.** The endpoint as reached from the page's own origin (`<origin>/mcp`), the server
and protocol version, the tool count, whether a bearer token is required (never the token), and a
warning when the server is bound to loopback. Copyable snippets, each tested as shown:

| Snippet | Goes into |
|---|---|
| VS Code, HTTP | `.vscode/mcp.json` (or the user configuration): `{"servers":{"awic":{"type":"http","url":"<origin>/mcp"}}}`; with a token, a `promptString` input with `password: true` and an `Authorization: Bearer ${input:awic-token}` header, so the token is never written into the file |
| VS Code, stdio | `podman exec -i awic awic mcp-stdio` as a `stdio` server (§8.1); the token, if any, through `AWIC_MCP_TOKEN` |
| Claude Code | `claude mcp add --transport http awic <origin>/mcp` |

**Sessions.** One row per `Mcp-Session-Id` (shown as its first 8 characters): client name and
version from `initialize`, negotiated protocol version, requests, tool calls, errors, the last
tool and when it was last seen. Ended sessions (deleted, or evicted as least recently used) stay
listed, greyed with the reason, up to 16 of them. Clicking a session filters the log to it.

**Message log.** Every exchange on `/mcp` — `initialize`, notifications, `tools/list`,
`tools/call`, refused requests, session ends — with time, session, tool, duration, ok/error and a
one-line summary (the arguments, or the error message). A row expands to the request and response
as pretty-printed JSON, each cut at 64 KB. Pictures a result carried (`render_view`) are lifted
out of the response text, replaced there by a placeholder, and shown as thumbnails under their
row whether it is expanded or not; a click enlarges one. Filters: free text, session, errors
only, screenshots only; *Follow* keeps the newest row in view.

**Memory.** Sessions and their counters are always tracked (bounded by §8.1's 32 sessions plus 16
ended). The message log is what costs memory, so it has two switches — *Log messages* and *Keep
screenshots* — set at start by `[mcp] log` / `log_images` (both on by default) and flippable at
runtime from the tab, plus *Clear*. The log is held under `[mcp] log_memory_mb` (default 16 MB,
pictures included), dropping the oldest entries first; a single entry over the bound gives up its
pictures. The tab shows memory used against the bound, the entry count, and how many were dropped.

**Wire.** Over `/ws` (§3.1), like every other module:

| Op | Result |
|---|---|
| `mcp.status` | `McpStatusResult`: endpoint path, bind, `auth_required`, versions, tool count, sessions, logging state |
| `mcp.log {after?, limit?}` | `McpLogResult`: entries with `seq > after`, oldest first, the newest `limit` (default 500) of them; `last_seq`; logging state |
| `mcp.image {seq, index}` | `McpImageResult {mime_type, data_b64}`; `not_found` when the entry was dropped or the picture not kept |
| `mcp.logging {enabled?, images?, clear?}` | `McpLoggingState`; logged with origin like any mutation |

Event `mcp.activity {seq, sessions_changed}` carries no entry, like `usb.changed`: the tab
pulls `mcp.log` from its last `seq` only while it is shown, so an idle browser costs nothing but
the event. The monitor hooks `/mcp`'s HTTP handler, the one place all MCP traffic arrives —
`awic mcp-stdio` included.

---

## 9. SCPI server (N-2, R-04)

A third transport over the same core: TCP **5025**, line-oriented, `\n`-terminated. Full control,
same capability as the other transports, and — since round 4 of the live test drove it through
**pyvisa** — the IEEE 488.2 and SCPI-99 *required* command sets in full, so a generic VISA client
can drive it with no AWIC-specific code. `tools/visa/README.md` describes the check (`make visa`).

```
*IDN?                          → AWIC,awic,0,0.1.0
*RST   *CLS   *OPC   *OPC?   *WAI
*TST?                          → 0        (nothing to self-test; see below)
*OPT?                          → 0        (no options; INST:CAT? is the bench)
*ESR?   *ESE 32   *ESE?   *SRE 4   *SRE?   *STB?
SYST:ERR?                      → 0,"No error"
SYST:VERS?                     → 1999.0
STAT:OPER?   STAT:QUES?   STAT:OPER:ENAB 512   STAT:PRES

INST:CAT?                      → "3-3.4.2","3-3.4.1.1"   (bus paths — what every op takes)
INST:SEL "3-3.4.2"
INST:SEL "scope"              also accepted: an unambiguous part of the instrument's label

USB:TREE?
USB:HUB "3-3":PORT3:POW OFF
USB:HUB "3-3":PORT3:POW?       → 0

ACQ:RATE 24E6
ACQ:CHAN (@1,2)
ACQ:RANG 2.5                   analog full-scale volts; ignored by digital instruments
ACQ:TRIG:EDGE:SOUR CH1 ; SLOP POS ; LEV 1.5
ACQ:STAR                       → capture handle
MEAS:FREQ? CH1                 → 1.0002E+3

REC:STAR                       → recording id (continuous, §2.8)
REC:SEL 3   REC:SEL?           adopt a recording started elsewhere (MCP, UI, another connection)
REC:SEAR? "D0 f=1k 100010x101" → 3.2140,5.8812      (match times, s)
REC:WAIT? "D2 rising",10       → 41.0021             (or 9.91E+37 on timeout)

SOUR:VOLT 3.3                  (refused above the §7.1 soft limit)
SOUR:OUTP ON
```

- `ACQ:STAR` answers its handle once the capture has **finished** (bounded: twice the nominal
  duration plus 5 s, at most 60 s), so the next line can measure it. `REC:*` queries act on
  the connection's current recording: the one it started, or the one `REC:SEL` picked —
  recordings outlive the connection that started them.
- Long and short forms accepted, case-insensitive, per SCPI convention. A **common command may
  carry parameters** (`*ESE 32`), which the parser once discarded — enough to make a standard VISA
  driver's startup sequence fail on its second line (live test B-29).
- **Status reporting is the real IEEE 488.2 model**, not a stub. `*ESR?` carries one bit per error
  class, taken from SCPI's own numbering — `-1xx` command error (bit 5), `-2xx` execution error
  (bit 4), `-3xx` and the server's positive device codes device-dependent (bit 3), `-4xx` query
  error (bit 2) — and reading it clears it, because a driver that polls `*ESR?` between commands
  to see whether each was accepted must not be shown the same event twice. `*ESE` gates those
  bits into the status byte's ESB, `*SRE` gates the status byte into MSS, and `*CLS` clears the
  register and the error queue together. There is no way to *assert* a service request: neither
  a TCP stream nor a terminal has an SRQ line, so `*SRE` is honoured in `*STB?` and nowhere else.
- `STATus`'s operation and questionable registers exist, as SCPI-99 requires, and read **0**:
  AWIC defines no operation or questionable conditions, because every device failure it knows
  about is an error queue entry, which the status byte already summarises through EAV. Their
  enable registers are stored so a driver's preset-and-enable sequence round-trips.
- `*TST?` answers `0`, IEEE 488.2's "no fault found", and tests nothing: the instruments are
  enumerated and their firmware uploaded long before a SCPI client connects, and one that failed
  that is missing from `INST:CAT?` rather than flagged by a self-test.
- **Termination is forgiving on input.** Responses end in `\n`, but a trailing `\r` on a command
  is ignored, because pyvisa's default `write_termination` for a socket resource is `\r\n` and an
  out-of-the-box VISA client would otherwise fail on every line.
- An **error queue** behind `SYST:ERR?`, because SCPI clients expect retrievable errors rather
  than inline ones. A failed query is therefore silent, which leaves a line-oriented client
  waiting for its own timeout; `SYST:COMM:ERR:INL ON` makes a failed **query** answer
  `ERR <code>,"<message>"` instead (still queued) — and a failed `ACQ:STAR` or `REC:STAR` too,
  the two setters that answer on success, since a client reads after those as well. Other
  failed setters stay silent either way, so a client that doesn't read after a setter cannot
  desynchronize.
- **No bulk sample data over SCPI**, for the same reason as §8.3 — `ACQ:STAR` yields a handle and
  `EXP:` commands write files. Dumping 2 MB into a line-oriented socket is a denial of service
  against the client.
- Allowlist and soft limits apply identically. This transport is not privileged.
- The engine is a crate (`awic-scpi`) behind two front ends: TCP 5025 and the `scpi` terminal
  source (§11.2), so the console in the browser is the same parser a script talks to.

`read_stb()` and a VISA device-clear are not AWIC's to offer over `TCPIP::…::SOCKET`: a raw socket
has no SRQ line, so the VISA layer refuses them for any instrument. `*STB?` and `*CLS` do the same
jobs in-band and are what a client should use.

You described the motivation as easier access for AI assistants via Python libraries. SCPI serves
that reasonably — its vocabulary is well represented in model training data, so an agent can often
drive it without documentation. It does overlap MCP, which is the better-typed path for agents;
SCPI is scheduled late (M8) accordingly, and a plain JSON REST view of the same operations is
nearly free alongside it for scripts that would rather use `requests`.

---

## 10. Export (Q-15)

CSV, VCD, PNG, JSON, Parquet. Reachable from all transports.

| Format | Notes |
|---|---|
| CSV | time column + per-channel columns; analog in volts, digital 0/1 |
| VCD | from the transition list; opens in GTKWave/PulseView, annotations as comments |
| JSON | metadata + decimated samples; small captures and agents |
| Parquet | via `arrow`; the right format for large captures and downstream analysis |
| PNG | server-side render from the capture buffer |

PNG is rendered independently of the browser renderers rather than by screenshotting a client, so
MCP and SCPI can produce images with no browser attached. Expect minor visual differences from the
live view.

---

## 11. Frontend (Q-06)

- **Vite + TypeScript**, strict. **Vanilla TS + web components**, no framework. External libraries
  where they earn their place (Plotly, uPlot, Parquet readers).
- Each module (`graph`, `usb`, `psu`, `decode`) is a directory that self-registers into the app
  shell, mirroring the backend split.
- State: a small observable store. Device and capture state **mirrors server events**; the server
  is authoritative and the client never maintains a parallel truth.
- Dev: `vite dev` on :5173 proxying `/ws` and `/api` to the backend on :8080.
  Prod: the Rust binary serves `dist/`.
- The USB tree view (N-1) is a component over `usb.tree`, with per-port toggles disabled and
  explained where a port is non-switchable or protected, and a lock to protect or unprotect it.

---

## 11.1 Theme

The UI offers a **light / dark switch** with three states — system (follows
`prefers-color-scheme`, the default), light, dark — persisted per browser. Plots follow the theme.

## 11.2 Terminal module (M14)

A **Terminal** tab built on **xterm.js** (`@xterm/xterm`, MIT — the terminal component VS Code
uses), with several sessions side by side as sub-tabs. A session is opened on a **source**:

| Source | What it is |
|---|---|
| `serial` | a detected UART (§11.2.1) — bidirectional |
| `scpi` | an interactive console on AWIC's own SCPI engine (§9) — the same parser as port 5025, with line editing and history in the browser |
| `log` | the server's log, formatted, with the last 1000 lines replayed on open; includes every mutation with its origin (§12), so the bench can see what an agent just did |
| `decode` | live protocol-decoder output (§6) on a recording's channels: UART bytes as text, I²C, SPI, CAN and 1-Wire transactions as one line each (CAN candump-style, 1-Wire one line per reset) |

```jsonc
{ "op": "term.sources" }
→ { "sources": [ { "source": "serial:3-3.4.4:1.1", "kind": "serial", "label": "ttyACM0 — Debug Probe (CMSIS-DAP)",
                   "device": "3-3.4.4", "available": true },
                 { "source": "log", "kind": "log", "label": "Server log", "available": true } ] }
{ "op": "term.open", "source": "serial:3-3.4.4:1.1", "serial": { "baud": 115200, "data_bits": 8,
                     "parity": "none", "stop_bits": 1 } }
→ { "session": 3 }
{ "op": "term.write", "session": 3, "data": "help\r" }          // or "data_b64" for raw bytes
{ "op": "term.close", "session": 3 }
{ "event": "term.state", "session": 3, "state": "open" | "waiting" | "closed", "msg": "..." }
{ "event": "term.sources" }                                     // the source list changed; re-read
```

Output arrives as terminal frames (§3.5). Serial sessions are **shared per port**: opening a port
that is already open joins the existing session, so two browser tabs or a browser and an agent see
the same byte stream instead of stealing it from each other. Every session keeps a 64 KiB
scrollback server-side, replayed on join.

### 11.2.1 Serial ports

- **Detection** from sysfs, per USB interface: `…/<intf>/tty/ttyACM<n>` (`cdc_acm`) and
  `…/<intf>/ttyUSB<n>` (`ftdi_sio`, `ch341`, `cp210x`, `pl2303`). The source id is the interface's
  bus path (`serial:3-3.4.4:1.1`), stable across re-enumeration like `DeviceId` (§2.3) — the tty
  name is not (`ttyACM0` and `ttyACM1` swap depending on enumeration order).
- **Access** through the host kernel's tty driver, opened at `/host/dev/tty*` (§1.3), not through
  usbfs: the kernel already has a driver for every bridge chip, and detaching it would take the
  port away from the host.
- Opened `O_NONBLOCK | O_NOCTTY` — a blocking open of a tty waits for carrier and hangs — then set
  raw with the requested baud and frame via termios, and `TIOCEXCL` so no other host process
  interleaves. No C: `rustix` termios plus tokio's `AsyncFd`.
- The USB tree shows a terminal icon on a port whose device has a UART; clicking it opens the
  session.
- **Port power**: when the device disappears (its port switched off, §4.4) the session goes to
  `waiting`, keeps its scrollback, and reopens by itself when the same interface path reappears.
- MCP tools (M7): `serial_list`, `serial_open`, `serial_write`, `serial_read`, with `serial_read`
  bounded in bytes per call (§8.3), `serial_expect` — wait until the output matches a regex,
  the text counterpart of `wait_for_pattern` (§2.9) — and `serial_close`. An MCP session leaves
  every serial session it joined when it ends (`DELETE /mcp` or eviction), as a WebSocket does
  when it closes; the port itself closes once nobody else is attached.

## 11.3 Layout

The main element — plot or terminal — owns the viewport; configuration moves out of its way:

- One **toolbar row** per module under the tab bar: device, mode (Single/Live), run/stop, and
  buttons that open **popovers** for settings (channels and ranges, trigger, acquisition,
  renderer). Popovers close on Escape and outside click; they never push the plot down.
- Measurements and search results live in a **collapsible side panel**, closed by default on narrow
  screens.
- **No page scroll** at 1280×720 or larger: the plot fills the remaining height, always from a
  definite-height container (the Plotly growth bug).
- Status (connection, recording span, memory) is compact text in the header, not a panel.
- The USB tree stays a full page; it is a list, not an instrument view.
- The MCP tab (§8.5) is two columns that each scroll on their own: connection, sessions and
  logging on the left, the message log on the right.

## 12. Security model (Q-08, R-01)

No authentication by default, free binding, LAN access wanted. Implemented as asked; consequences
recorded once here rather than discovered later.

- An **optional token** (R-01): if `auth.token` is set in config, both the web UI and `/mcp`
  require it. Unset by default, so the out-of-the-box behaviour is exactly what you chose.
- The service can cut USB power and command PSU output. These are **physical effects**, so
  exposure is not only a confidentiality question.
- Soft limits and port protection are enforced in core and are therefore the real boundary.
  They constrain an anonymous LAN client exactly as they constrain you.
- `--net=host` (§1.3) means no port isolation from the host either.

Every mutation from every transport is logged with origin and pushed to the UI as an event.
Observability is the compensating control when access control is absent by choice.

**Browser origins.** No auth must not mean "any website can drive the bench". Browsers apply no
same-origin policy to WebSocket upgrades, and a `text/plain` POST needs no CORS preflight, so a
page the user merely visits could open `ws://127.0.0.1:8080/ws` and switch USB power. `/ws` and
`/mcp` therefore refuse a request whose `Origin` header is neither the server's own (its `Host`) nor
listed in `[server] allowed_origins` (default: the Vite dev server). Requests without `Origin` —
scripts, MCP clients, `awic mcp-stdio` — are unaffected; this is not authentication, the token is.

**Bounded sessions.** `/mcp` keeps at most 32 sessions and evicts the least recently used; a client
whose session was evicted re-initializes, as the MCP transport expects.

---

## 13. Firmware handling (R-07)

Resolved in order, first hit wins:

1. **Host override** — `/etc/awic/firmware`, i.e. `config/firmware/` on the host via the existing
   config mount. Kept apart from the image's own copy so the log can say which one was used.
2. **Baked into the image** — from `sigrok-firmware-fx2lafw`, covering both instruments. Default
   path; needs no network.
3. **Auto-download** — *not implemented yet*: `firmware.auto_download = true` is refused with
   `AutoDownloadNotImplemented`. When it comes: opt-in, from pinned URLs, **verified against
   committed SHA-256 checksums before use**.

The checksum requirement on step 3 is not optional: these blobs are executed by hardware on your
bench, so an unverified download is a code-execution path into the instrument. The resolution
chain logs which source supplied each blob.

---

## 14. Testing

- **Fake backend** (`awic-fake`) — synthetic analog and digital signals, built in M2 **before any
  real driver**. Makes protocol, UI, MCP and SCPI testable in CI with no hardware, and is why M4
  can slip without blocking M5–M9.
- **`sigrok-cli` as a test oracle** — not linked, not shipped, but kept in the dev toolchain.
  Capture the same signal with sigrok-cli and with AWIC, compare. It is already confirmed working
  on this bench, which makes it trustworthy ground truth for native driver bring-up.
- Protocol round-trip tests over a real WebSocket.
- Decoder tests against committed known-good capture fixtures.
- Hardware: `#[ignore]` tests per driver, run on the bench.

---

## 15. Design decisions

The decisions that shape AWIC, as they stand. Each ID is cited where the decision applies. Answers
that were later reversed are not listed; where a decision replaced an earlier one, it says so.

### Platform

| ID | Decision |
|---|---|
| **Q-01** | Host access through **udev rules** (§1.2) as the default; rootful `podman --privileged` as the documented fallback. |
| **R-02** | **`debian:trixie-slim`** base image and **native Rust drivers**. libsigrok was dropped (2026-09-11): its last release is from 2019, it linked GPL-3.0 and C into the process, and its GLib main loop would have needed a thread bridge to tokio. With it went libusb, so USB is **`nusb`** (pure Rust, async; `rusb` the fallback) and there is no C library in the process. `sigrok-cli` stays as a test oracle, never linked. The `Backend` trait keeps drivers replaceable (§2.2). Replaces Q-02 (UBI9), Q-03 (rusb) and Q-05 (libsigrok, GPL-3.0). |
| **Q-04** | **Native hub port power** (§4.2), no `uhubctl` binary. |
| **Q-07** | `--net=host` by default for netlink hot-plug, with a `/sys` polling fallback (§4.3). |
| **R-07** | Firmware: host override, then baked into the image, then opt-in download verified against committed SHA-256 sums (§13). |
| **D-08** | UARTs through the **host kernel's tty driver**, via `/dev` bind-mounted at `/host/dev` (§11.2.1); speaking CDC-ACM over usbfs was rejected — a driver per bridge chip, and it takes the port from the host. |
| **D-11** | **License MPL-2.0** (2026-10-09): changes to AWIC's own files stay open; linking AWIC into other software, under any license, stays allowed. |
| **Q-18** | The name is `awic`. |

### Instruments and USB

| ID | Decision |
|---|---|
| **R-03** | The oscilloscope is a **scope only**; the standalone fx2lafw analyzer is the logic analyzer. The scope's logic mode is a physical switch, so there is no software mode control. In logic mode it enumerates as `0925:3881`, the logic analyzer's ID, so identity is the bus path and the UI labels by place (§5.1). |
| **N-1** | The USB module shows the **device tree** with per-port power (§4). |
| **D-01** | **Switchable ports are discovered**, not configured; protection is derived from what is attached, and the user's lock/unlock choices persist as session state (§4.4). |
| **Q-10** | **No confirmation gates.** Configurable soft limits, enforced in core for every transport, are the guardrail (§7.1). |
| **Q-13**, **R-05** | PSU: a `PowerSupply` trait and a **generic SCPI-over-serial** template driver (§7). |

### Data

| ID | Decision |
|---|---|
| **Q-14** | **Native Rust decoders** (§6), not libsigrokdecode (which would bring a Python runtime). Decoders, measurements and the stores work on AWIC's own sample buffers. |
| **Q-15** | Export formats: CSV, VCD, PNG, JSON, Parquet (§10). `.sr` is not offered. |
| **Q-16** | Captures in an **in-memory ring**; persistence is by explicit export (§2.7). |
| **D-04** | **Live recorder**: continuous acquisition into a server-owned, bounded, compressed in-memory history — run-length encoded digital, losslessly compressed analog blocks, a multi-resolution summary tree — streamed to the plot as conflated view frames (§2.8, §3.4). |
| **S-03** | Recording runs **on request** (Live button, `rec.start`, an agent tool), not always. |
| **S-04** | Recordings stay **in memory**, 256 MB by default, configurable; snapshots and export make data permanent. |
| **S-06** | Any analog rate the device sustains may be recorded; the toolbar shows the retention live. |
| **D-06** | **Pattern search and wait** in a one-line language: bit patterns, edges, pulse widths, multi-channel states, clocked patterns, analog levels (§2.9). Searches cost `O(edges)` on the RLE data, pruned by the summary tree. |
| **S-01** | Bit patterns are **edge-anchored**, like a UART receiver: every edge re-synchronises the bit grid, a run of length d is round(d·f) cells, glitches under half a cell are absorbed (§2.9.2). |
| **S-02** | **Clocked patterns**: one channel read on another's edges, `D1 clk=D0 rising 1010x`. |
| **D-10** | Decoders and pattern matchers count in **sample indices**, not nanoseconds. |

### Transports and UI

| ID | Decision |
|---|---|
| **Q-09** | MCP over **streamable HTTP at `/mcp`**, plus a stdio proxy (§8.1). |
| **N-2**, **R-04** | An **SCPI server** on TCP 5025, IEEE 488.2 with full control, as a third transport — for scripts and AI access (§9). |
| **R-01** | **Optional token**, off by default, for both the UI and `/mcp`; LAN binding allowed (§12). Replaces Q-08 (no authentication at all). |
| **Q-06** | Frontend in **vanilla TypeScript and web components**, external libraries where they earn their place; switchable renderers (§5.3, §11). |
| **D-05** | **WebGL2 is the graph's default renderer**; Plotly stays selectable (S-07). Replaces R-06 (Plotly by default). |
| **D-07** | **Terminal module** on xterm.js with four sources: serial UARTs (absorbing D-02, the serial console), the SCPI console on AWIC's own engine (S-05), the server log, and live decoder output (§11.2). |
| **S-08** | The log terminal shows info and above plus every mutation with its origin, with a level filter. |
| **D-03** | Light and dark theme (§11.1). |
| **D-09** | **Compact layout**: toolbar and popovers, no page scroll at 1280×720 (§11.3). |
