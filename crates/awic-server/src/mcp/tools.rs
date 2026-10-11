//! The MCP tool catalog (SPECIFICATION.md §8.2) and the dispatcher that
//! turns one `tools/call` into a call against [`McpHost`].
//!
//! Every description here is written for an agent that has read nothing
//! else about AWIC: what the tool does, the units of every numeric field,
//! the limits that apply, and one worked example. `tools/list` is the
//! *only* documentation an MCP client is guaranteed to have read (the task
//! brief's own end-to-end check drives the server using nothing else), so
//! anything not said here effectively doesn't exist to the agent calling it.

use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use awic_proto::{MeasurementKind, Parity, SerialSettings, StopBits, Trigger};

use super::host::McpHost;
use super::protocol::{tool_err, tool_ok, tool_ok_with_image};

/// Said when `power_cycle_usb_port` brings nothing back (B-50).
const NOTHING_ENUMERATED: &str = "nothing enumerated on this port. Either nothing is plugged in, \
    or its power is cut where software cannot reach it: some hubs have a power button per port, \
    and a port switched off there still reads as on in usb_tree. If a device was attached here, \
    ask a person to check the hub.";

pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

/// `tools/list`'s payload: every tool this server implements, with a JSON
/// Schema an MCP client can validate arguments against before ever calling
/// `tools/call` — and that a human reviewer can read as the spec for what
/// each tool accepts.
pub fn tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "list_instruments",
            description: "List every instrument AWIC currently knows about: its bus-path id \
                (e.g. \"3-3.4.2\"), USB VID:PID, human label, lifecycle state \
                (detected/flashing/ready/error), and capabilities (analog channels+bits+rates+ranges, \
                or digital channels+max rate). Call this first — every other tool that takes a \
                `device` argument takes one of these ids. Takes no arguments. \
                Example call: {}. Example result: {\"devices\":[{\"id\":\"3-3.4.2\",\"vid\":1237,\
                \"pid\":24610,\"label\":\"FX2 oscilloscope\",\"state\":\"ready\",\"capabilities\":\
                [{\"kind\":\"analog\",\"channels\":2,\"bits\":8,\"rates_hz\":[48000000,...],\
                \"ranges_v\":[0.25,5.0]}]}]}",
            input_schema: object_schema(vec![], vec![]),
        },
        ToolDef {
            name: "usb_tree",
            description: "Show the full USB hub topology: every hub and leaf device, each hub's \
                downstream ports (1-based numbers) with power state (on/off/unknown), whether that \
                port supports power switching at all (`power_switchable`), and whether it is \
                currently protected against `set_usb_power` (`protected`, with `protection` \
                explaining why — e.g. \"a hub is attached\" or \"HID device\"). Use this to find \
                the `hub`/`port` arguments `set_usb_power` and `set_usb_port_protected` need. Takes \
                no arguments. Example: a hub \"3-3\" whose port 4 has sub-hub \"3-3.4\" attached \
                shows that port as protected (cutting it would cut everything behind the sub-hub). \
                `power` is what the hub controller reports: a hub with its own per-port power \
                buttons can cut a port that still reads as on, and no command here turns it back \
                on — if a device that was attached is missing, ask a person to check the hub.",
            input_schema: object_schema(vec![], vec![]),
        },
        ToolDef {
            name: "set_usb_power",
            description: "Switch one USB hub port on or off. `hub` is a bus path like \"3-3\" or \
                \"3-3.4\" (see `usb_tree`); `port` is the 1-based port number on that hub; `on` is \
                the desired power state. Refused with an explanation if the port is not \
                power-switchable, or if it is protected (a hub, keyboard/mouse, disk, or wireless \
                adapter is attached, or a human locked it) — protection cannot be bypassed from \
                here; use `set_usb_port_protected` first if you are sure. Powering a port back on \
                does not mean the device is immediately usable: an unprogrammed instrument \
                re-enumerates and reflashes over 2-4 seconds; re-check `list_instruments` afterwards. \
                Look the port up in `usb_tree` first and check which device it feeds: port numbers \
                differ from bench to bench, and cutting the wrong one resets the device under test. \
                Example shape: {\"hub\":\"<hub path>\",\"port\":<n>,\"on\":false}.",
            input_schema: object_schema(
                vec![
                    ("hub", json!({"type":"string","description":"Hub bus path, e.g. \"3-3.4\""})),
                    ("port", json!({"type":"integer","minimum":1,"maximum":255,"description":"1-based port number"})),
                    ("on", json!({"type":"boolean","description":"true = power on, false = power off"})),
                ],
                vec!["hub", "port", "on"],
            ),
        },
        ToolDef {
            name: "set_usb_port_protected",
            description: "Lock or unlock a USB hub port against `set_usb_power`, or return it to \
                the automatic rule. `mode` is one of \"lock\" (refuse power changes regardless of \
                what is attached), \"unlock\" (allow power changes even if the automatic rule would \
                protect it — attaching a hub, keyboard, disk, etc.), or \"auto\" (go back to the \
                automatic, attachment-derived rule). This is a deliberate override: unlocking a port \
                feeding a hub means a later `set_usb_power` off will cut every device behind it. \
                Example: {\"hub\":\"3-3\",\"port\":4,\"mode\":\"auto\"} restores the default rule \
                for that port (protected, because a hub is attached there).",
            input_schema: object_schema(
                vec![
                    ("hub", json!({"type":"string"})),
                    ("port", json!({"type":"integer","minimum":1,"maximum":255})),
                    ("mode", json!({"type":"string","enum":["lock","unlock","auto"]})),
                ],
                vec!["hub", "port", "mode"],
            ),
        },
        ToolDef {
            name: "power_cycle_usb_port",
            description: "Power-cycle one USB port in a single call: switch it off, wait `off_ms` \
                (default 1000, enough for a board's supply rails to discharge), switch it back on, \
                then wait up to `wait_s` (default 5, maximum 30) for a device to enumerate on it \
                again. Returns what came back (`device`, with its VID:PID and product string) and \
                how long it took (`enumerated_after_s`), or no `device` if nothing appeared in \
                time. This is the reliable way to reset a device under test: done with two \
                `set_usb_power` calls, a failure in between leaves the port off. Same protection \
                rules as `set_usb_power` — a protected port is refused. An instrument that \
                reflashes on power-up may need a few seconds more before `list_instruments` shows \
                it ready. Check `usb_tree` first for what the port feeds; a port labelled as the \
                device under test resets the board you are measuring. Example shape: \
                {\"hub\":\"<hub path>\",\"port\":<n>,\"off_ms\":1500}.",
            input_schema: object_schema(
                vec![
                    ("hub", json!({"type":"string","description":"Hub bus path, e.g. \"3-3.4\""})),
                    ("port", json!({"type":"integer","minimum":1,"maximum":255})),
                    ("off_ms", json!({"type":"integer","minimum":50,"maximum":30000,"description":"How long to stay off; default 1000"})),
                    ("wait_s", json!({"type":"number","minimum":0,"maximum":30,"description":"How long to wait for re-enumeration; default 5"})),
                ],
                vec!["hub", "port"],
            ),
        },
        ToolDef {
            name: "label_usb_port",
            description: "Record what a USB port feeds, so anyone (including you, later) sees it \
                in `usb_tree` before switching that port. `label` is free text such as \"DUT: \
                RP2350 board\" (null clears it); `dut` marks the port as feeding the device under \
                test. Bench wiring differs from bench to bench and a port number says nothing \
                about what is attached, so labelling the board you are measuring is worth doing as \
                soon as you identify it. Persisted across restarts as session state, like port \
                locks. Example: {\"hub\":\"3-3.4\",\"port\":4,\"label\":\"DUT: RP2350 test \
                board\",\"dut\":true}.",
            input_schema: object_schema(
                vec![
                    ("hub", json!({"type":"string"})),
                    ("port", json!({"type":"integer","minimum":1,"maximum":255})),
                    ("label", json!({"type":["string","null"],"description":"What the port feeds; null clears the label"})),
                    ("dut", json!({"type":"boolean","description":"Mark (or unmark) as the device under test; omitted keeps the current mark"})),
                ],
                vec!["hub", "port"],
            ),
        },
        ToolDef {
            name: "configure_acquisition",
            description: "Configure a device for a finite (non-continuous) acquisition: sample \
                rate, channels, and sample count. `device` is a bus-path id from `list_instruments`. \
                `rate_hz` is the requested sample rate in Hz; the hardware may round it down to its \
                own ladder (e.g. the FX2 oscilloscope supports 48/30/24/16/8/4/1 MHz and \
                500/200/100 kHz) — the returned `cfg.rate_hz` is what will actually run, and every \
                later time calculation must use that, not the request. `channels` are 0-based \
                hardware channel numbers (digital: 0-7; the oscilloscope: 0-1). `samples` is the total \
                sample count to capture (ignored by `start_recording`, which runs continuously \
                instead). `trigger` is optional: {\"kind\":\"edge\",\"channel\":0,\
                \"slope\":\"rising\"|\"falling\",\"level\":1.5} (`level` in volts, required on an \
                analog channel and left out on a digital one, where an edge is a change of bit; \
                `kind` may be omitted — it defaults to \"edge\", the only kind). \
                `ranges_v` optionally sets each analog channel's full-scale range in volts, one \
                entry per `channels` entry, from the device's own `ranges_v` capability. Must be \
                called before `capture`. Example: {\"device\":\"3-3.4.2\",\"rate_hz\":24000000,\
                \"channels\":[0,1],\"samples\":1048576}.",
            input_schema: object_schema(
                vec![
                    ("device", json!({"type":"string"})),
                    ("rate_hz", json!({"type":"integer","minimum":1,"description":"Requested sample rate, Hz"})),
                    ("channels", json!({"type":"array","items":{"type":"integer","minimum":0,"maximum":255}})),
                    ("samples", json!({"type":"integer","minimum":0,"description":"Total samples to capture"})),
                    ("trigger", json!({
                        "type":"object","description":"Optional edge trigger. The capture is \
                            the `samples` around the first matching edge, which lands at sample \
                            samples/2; `capture` reports `triggered` and where, or fails with \
                            trigger_not_fired after timeout_s",
                        "properties": {
                            "kind": {"type":"string","enum":["edge"],"default":"edge","description":"Optional, default \"edge\""},
                            "channel": {"type":"integer","minimum":0,"description":"Hardware channel: CH1 is 0, D4 is 4. Must be one of `channels`"},
                            "slope": {"type":"string","enum":["rising","falling"]},
                            "level": {"type":"number","description":"Volts. Required on an analog channel; leave it out on a digital one"}
                        },
                        "required": ["channel","slope"]
                    })),
                    ("ranges_v", json!({"type":"array","items":{"type":"number"},"description":"Analog full-scale range per channel, volts"})),
                ],
                vec!["device", "rate_hz", "channels", "samples"],
            ),
        },
        ToolDef {
            name: "capture",
            description: "Run one finite acquisition on a device already set up with \
                `configure_acquisition`, and wait for it to finish. Returns a capture handle \
                (`capture://...`) plus **summary statistics only, never raw samples** — for analog \
                channels: Vpp/Vmin/Vmax/Vavg/Vrms (volts), frequency (Hz), duty (percent); for \
                digital channels: frequency, duty, high/low pulse width (seconds), edge count. Use \
                `read_capture` for a decimated waveform preview or `measure` for one specific \
                statistic. `timeout_s` bounds how long to wait for the acquisition to finish \
                (default: estimated from samples/rate, at least 5s; hard maximum 60s) — if it \
                elapses the capture may still be running server-side. Handles do not last \
                forever: the capture store keeps at most 32 captures / 512 MB, oldest dropped \
                first (a handle that was dropped answers `capture_evicted`, not \
                `capture_not_found`). Example: \
                {\"device\":\"3-3.4.2\"} after configuring 1,048,576 samples at 24 MHz might return \
                {\"capture\":\"capture://...\",\"channels\":[{\"channel\":0,\"results\":\
                [{\"kind\":\"frequency\",\"value\":999.98,\"unit\":\"Hz\"}, ...]}]}.",
            input_schema: object_schema(
                vec![
                    ("device", json!({"type":"string"})),
                    ("timeout_s", json!({"type":"number","minimum":0.1,"maximum":60,"description":"Seconds to wait for completion; default is estimated from the configured sample count and rate"})),
                ],
                vec!["device"],
            ),
        },
        ToolDef {
            name: "read_capture",
            description: "Read a decimated preview of one channel of a capture (from `capture`, \
                `snapshot_recording`, or any `capture://...` handle) — never full-resolution \
                samples (SPEC §8.3). `channel` is the hardware channel number. Returns two \
                parallel arrays, `min` and `max`, of up to `max_points` entries each — bucket i is \
                (`min[i]`, `max[i]`) — evenly bucketed in time across the range read; the values \
                are volts for analog channels (unit \"V\") or 0.0/1.0 for \
                digital (unit \"\", where (0,1) means the channel toggled within that bucket — a \
                glitch is never averaged away). `max_points` defaults to 512 and is capped at 4096 \
                server-side regardless of what is requested. `truncated: true` means the capture has \
                more resolution than was returned. Example: {\"capture\":\"capture://...\",\
                \"channel\":0,\"max_points\":100} for a quick shape check before deciding whether to \
                call `measure` for exact numbers. `t0_s`/`t1_s` (seconds from the capture's start, \
                default the whole capture) choose the range that is bucketed, so a short window \
                comes back at full resolution without re-capturing: {\"capture\":\"capture://...\",\
                \"channel\":0,\"t0_s\":0.010,\"t1_s\":0.011} reads one millisecond starting at \
                10 ms. `sample0`/`sample1` give the same window as sample numbers, as `decode` \
                reports a transaction's `start_sample`/`end_sample`.",
            input_schema: object_schema(
                vec![
                    ("capture", json!({"type":"string"})),
                    ("channel", json!({"type":"integer","minimum":0,"maximum":255})),
                    ("max_points", json!({"type":"integer","minimum":1,"maximum":4096,"description":"Default 512, hard maximum 4096"})),
                    ("t0_s", json!({"type":"number","minimum":0,"description":"Window start, seconds from the capture's start; default 0"})),
                    ("t1_s", json!({"type":"number","minimum":0,"description":"Window end, seconds; default the end of the capture"})),
                    ("sample0", json!({"type":"integer","minimum":0,"description":"Window start as a sample number from the capture's start (as `decode`'s start_sample/end_sample); instead of t0_s/t1_s"})),
                    ("sample1", json!({"type":"integer","minimum":1,"description":"Window end as a sample number; instead of t0_s/t1_s"})),
                ],
                vec!["capture", "channel"],
            ),
        },
        ToolDef {
            name: "measure",
            description: "Compute one or more scalar measurements on one channel of a capture, on \
                full-resolution data. `measurements` is a list of: \"vpp\", \"vmin\", \"vmax\", \
                \"vavg\", \"vrms\" (analog, volts); \"frequency\" (Hz), \"period\" (seconds), \"duty\" \
                (percent 0-100, analog or digital); \"rise_time\", \"fall_time\" (seconds, analog); \
                \"pulse_width_high\", \"pulse_width_low\" (seconds, digital); \"edge_count\" \
                (digital). A measurement that does not apply to the capture's type, or cannot be \
                computed, comes back as {\"kind\":...,\"unavailable\":\"<reason>\"} rather than \
                a made-up number — check for that field rather than assuming every entry has a \
                `value`. Two common reasons: fewer than two edges in the capture, and an analog \
                signal inside the noise floor (under 4 ADC codes peak-to-peak), where a \
                frequency would describe the converter's dither rather than the signal — \
                amplitude is still reported there. A value can carry a `note` it must not be read \
                without — e.g. on NRZ data (UART, CAN) frequency/period/duty are averages over \
                runs of different lengths, and the note says so and gives the bit rate. Example: \
                {\"capture\":\"capture://...\",\"channel\":0,\"measurements\":[\"frequency\",\"vpp\"]} \
                -> {\"results\":[{\"kind\":\"frequency\",\"value\":1000.2,\"unit\":\"Hz\"},\
                {\"kind\":\"vpp\",\"value\":3.3,\"unit\":\"V\"}]}.",
            input_schema: object_schema(
                vec![
                    ("capture", json!({"type":"string"})),
                    ("channel", json!({"type":"integer","minimum":0,"maximum":255})),
                    ("measurements", json!({"type":"array","items":{"type":"string","enum":
                        ["vpp","vmin","vmax","vavg","vrms","frequency","period","duty",
                         "rise_time","fall_time","pulse_width_high","pulse_width_low","edge_count"]}})),
                ],
                vec!["capture", "channel", "measurements"],
            ),
        },
        ToolDef {
            name: "decode",
            description: "Decode a protocol (UART, I2C, SPI, CAN or 1-Wire — see `list_decoders` for exact \
                channel roles and options) over `[t0_s, t1_s)` of either a finished `capture` (default: \
                all of it; seconds from its start, as in `read_capture`) or a `rec` (default: its \
                whole retained span) — give exactly one of `capture`/`rec`. A window opened \
                mid-frame waits for the next frame start. \
                `channels` maps each decoder role to a hardware channel number, e.g. UART needs \
                [{\"role\":\"rx\",\"channel\":0}]; I2C needs `scl`+`sda`; SPI needs `clk` plus any \
                of `mosi`/`miso`/`cs`; CAN needs one line, `can`; 1-Wire one line, `owr`. `options` overrides a decoder's defaults from `list_decoders` \
                (e.g. [{\"id\":\"baud\",\"value\":9600}]) — anything not given keeps its default. \
                For UART, `baud` may be \"auto\" (or 0) to detect the rate from the line itself; \
                likewise CAN's `bitrate` (snapped to a standard rate); the result then reports \
                `detected_baud`. CAN decodes classic CAN 2.0A/B frames from one digital line \
                (RX pin or CANL: `polarity` `dominant_low`, the default; CANH: \
                `dominant_high`), checks the CRC, and reports unacknowledged frames and error \
                frames. 1-Wire (decoder `one_wire`) times the master's low pulses at \
                standard or overdrive speed (at least 267 kS/s, 1.34 MS/s for overdrive): \
                resets with or without a presence pulse, the ROM command, the 64-bit ROM \
                code (named like Linux's w1, `28-00000A1B2C3D`, CRC-8 checked; also read out \
                of a Search ROM), then data bytes. \
                A `capture` from an oscilloscope (analog) works too: each role's line is cut into \
                logic levels first, at a threshold found automatically per channel (halfway \
                between its two dominant voltage levels, with a dead band of 20 % of the swing, \
                at least 1.5 ADC codes, against noise) and reported in the result's `note`. For \
                CAN on a difference (`minus`) the threshold is at most the standard 0.7 V \
                (ISO 11898-2), so a weakly driven ACK slot still reads dominant; the `note` also \
                says when one side of a difference never moves (a probe off its wire). \
                Override it with the options `threshold_mv` and `hysteresis_mv` (integer \
                millivolts; hysteresis is the total dead-band width) — only valid on analog \
                captures, as is a channel mapping's `minus`, which makes the role's line the \
                difference `channel - minus` (a differential pair, e.g. CANH - CANL). A channel \
                that never moves reads as constant, and the `note` says so. `rec` is digital \
                only; `snapshot_recording` an analog recording first. Two output shapes. \
                `format`:\"transactions\" (best for reading a bus) returns one entry per I2C \
                START..STOP, UART burst, SPI chip-select window, CAN frame or 1-Wire reset: {\"t_s\",\"address\",\"read\",\
                \"data\" (hex, e.g. \"40 00 1F\"),\"count\",\"text\" (UART only),\"acked\" \
                (I2C and CAN: true when every byte / the frame was ACKed, false on any NACK / \
                an unacknowledged frame; 1-Wire: a presence pulse answered the reset),\
                \"notes\" (NACKs, framing errors; CAN: extended, remote, no ACK, CRC error, \
                error frame; 1-Wire: the ROM command and ROM code)}, plus a result-level `summary` {\"transactions\",\"nacks\",\
                \"errors\",\"by_address\":[{\"address\",\"transactions\",\"nacks\"}],\"folded\",\
                \"bursts\":{\"count\",\"transactions_min\",\"transactions_max\",\"period_s\",\
                \"duration_s\",\"gap_s\"}} over the returned transactions (for CAN `address` is the identifier, up to 29 bits, and \
                `nacks` counts unacknowledged frames; for 1-Wire `address` is the ROM's family \
                code and `nacks` counts resets without presence) — read NACK counts there, not from \
                missing notes. Identical transactions in a row come back as one entry with \
                `repeat` (its `end_sample` is the last one's; `fold`:false lists each), and \
                `summary.transactions` still counts every one; `bursts` appears when the \
                transactions come in bursts separated by idle gaps. A UART burst (or SPI burst \
                without CS) longer than `max_bytes` (default 256) continues in the next entry, \
                noted \"continues the previous transaction\". An SPI CS window whose clock \
                pauses for well over a word notes it: two transfers may share it because \
                the CS release between them was shorter than a sample. One 1 KB display write is ~66 transactions there against \
                ~11000 annotations. `format`:\"annotations\" (the default) returns each decoded \
                unit: {\"start_sample\",\"end_sample\",\"level\" (0=frame,1=field,2=bit),\
                \"kind\":{...},\"text\"}; `max_level` bounds the nesting returned and defaults \
                to 1 (frames and fields, no per-bit annotations such as I2C ACK bits) — pass 2 \
                to get those too. Never bulk samples either way. `max_annotations` defaults to \
                500, hard maximum 5000, and bounds transactions the same way; `truncated`/\
                `omitted` say whether and how many were cut off — when `truncated` is true, \
                decode a shorter range or ask for transactions rather than assuming you saw \
                everything. With `rec`: If the recording is no longer live, the result carries a `note` saying when and why it ended — nothing after that was recorded, so an empty result there does not mean nothing happened. The `note` also names any gap in the range: a span where the device stalled and was reset and the recording resumed after it, which holds the last level repeated, not what the line did. \
                Example: {\"rec\":1,\"decoder\":\"i2c\",\"channels\":[{\
                \"role\":\"scl\",\"channel\":6},{\"role\":\"sda\",\"channel\":4}],\
                \"format\":\"transactions\",\"t0_s\":1.0,\"t1_s\":1.05} -> \
                {\"transactions\":[{\"t_s\":1.0021,\"address\":60,\"read\":false,\
                \"data\":\"40 00 7F\",\"count\":3}],...}.",
            input_schema: object_schema(
                vec![
                    ("capture", json!({"type":"string","description":"A capture:// handle; give this or `rec`, not both"})),
                    ("rec", json!({"type":"integer","minimum":0,"description":"A recording id; give this or `capture`, not both"})),
                    ("t0_s", json!({"type":"number","minimum":0,"description":"Window start, seconds: from the capture's start (default 0), or the recording's (default: start of retained span)"})),
                    ("t1_s", json!({"type":"number","minimum":0,"description":"Window end, seconds; default the end of the capture or of the retained span"})),
                    ("decoder", json!({"type":"string","enum":["uart","i2c","spi","can","one_wire"]})),
                    ("channels", json!({
                        "type":"array","description":"Role -> hardware channel bindings",
                        "items": {
                            "type":"object",
                            "properties": {
                                "role": {"type":"string","enum":["rx","scl","sda","clk","mosi","miso","cs","can","owr"]},
                                "channel": {"type":"integer","minimum":0,"maximum":255},
                                "minus": {"type":"integer","minimum":0,"maximum":255,"description":"Analog capture only: the role's line is `channel - minus` (differential)"}
                            },
                            "required": ["role","channel"]
                        }
                    })),
                    ("options", json!({
                        "type":"array","description":"Overrides for this decoder's default options (see list_decoders). On analog captures also `threshold_mv` and `hysteresis_mv` (integer millivolts)",
                        "items": {
                            "type":"object",
                            "properties": {
                                "id": {"type":"string"},
                                "value": {"description":"An integer, boolean or string, matching the option's type"}
                            },
                            "required": ["id","value"]
                        }
                    })),
                    ("max_annotations", json!({"type":"integer","minimum":1,"maximum":5000,"description":"Default 500, hard maximum 5000; bounds transactions too"})),
                    ("max_level", json!({"type":"integer","minimum":0,"maximum":2,"description":"annotations format only: deepest nesting to return (0 frames, 1 +fields, 2 +bits). Default 1"})),
                    ("format", json!({"type":"string","enum":["annotations","transactions"],"description":"\"transactions\" groups annotations into one entry per bus transaction; default \"annotations\""})),
                    ("max_bytes", json!({"type":"integer","minimum":1,"maximum":4096,"description":"transactions format: the most data bytes in one UART burst (or SPI burst without CS) before it continues in the next entry. Default 256"})),
                    ("fold", json!({"type":"boolean","description":"transactions format: fold identical consecutive transactions into one entry with `repeat`. Default true"})),
                ],
                vec!["decoder", "channels"],
            ),
        },
        ToolDef {
            name: "survey_channels",
            description: "Answer \"which pin does what\" for a digital instrument in one call: per \
                channel its resting level, edge count, shortest high/low pulse, glitch count, burst \
                rhythm and a one-line `summary`, plus `guesses` naming the protocols found — a UART \
                line with its detected baud rate, an I2C clock/data pair with its bit rate, an \
                SPI bus (`clk`, its `data` lines — MOSI and MISO, which timing cannot tell \
                apart — `cs` when one brackets the transfers and `cs_active_high` when it is asserted high, `mode` 0-3 and `clock_hz`), a \
                free-running clock with its frequency. Start here on an unknown board, then feed \
                the guesses straight into `decode`. Give exactly one of: `device` (takes a short \
                capture of every digital channel itself — `duration_s` default 1.0, `rate_hz` \
                default 4000000), `capture` (a handle you already have), or `rec` with optional \
                `t0_s`/`t1_s`. Digital instruments only; for an analog capture, `capture` already \
                reports Vpp/frequency/duty per channel. A channel that never toggles is reported \
                as constant high or low, which is itself the answer for an idle bus line, and one \
                that only shows two-sample spikes is called out as floating or crosstalk rather \
                than decoded. With `rec`: If the recording is no longer live, the result carries a `note` saying when and why it ended — nothing after that was recorded, so an empty result there does not mean nothing happened. The `note` also names any gap in the range: a span where the device stalled and was reset and the recording resumed after it, which holds the last level repeated, not what the line did. \
                Example: {\"device\":\"3-3.4.1.1\"} -> {\"channels\":[{\
                \"channel\":6,\"summary\":\"I2C SCL (data on D4) at ~364.000 kHz, 2 \
                transactions\",...},...],\"guesses\":[{\"protocol\":\"i2c\",\"scl\":6,\
                \"sda\":4,\"starts\":2,\"clock_hz\":364000},{\"protocol\":\"uart\",\
                \"channel\":3,\"baud\":115200,\"fit\":0.99,\"inverted\":false}]}.",
            input_schema: object_schema(
                vec![
                    ("device", json!({"type":"string","description":"Take a fresh capture on this digital instrument and survey it"})),
                    ("capture", json!({"type":"string","description":"Survey an existing capture:// handle"})),
                    ("rec", json!({"type":"integer","minimum":0,"description":"Survey a recording"})),
                    ("t0_s", json!({"type":"number","minimum":0,"description":"capture or rec: window start, seconds; default the start of the capture or of the retained span"})),
                    ("t1_s", json!({"type":"number","minimum":0,"description":"capture or rec: window end, seconds; default the end of the capture or of the retained span"})),
                    ("duration_s", json!({"type":"number","minimum":0.001,"maximum":10,"description":"device only; seconds to capture, default 1.0"})),
                    ("rate_hz", json!({"type":"integer","minimum":1,"description":"device only; sample rate, default 4000000 (rounded to what the device supports)"})),
                ],
                vec![],
            ),
        },
        ToolDef {
            name: "render_view",
            description: "Draw a capture and get back both the picture and the numbers it \
                measures — the closest thing to looking at a scope screen. Returns an MCP image \
                block (SVG) plus `metrics`, a list of {name, value, unit, text}: `value` is in the \
                base unit (volts, seconds, hertz) for arithmetic, `text` is how the picture labels \
                it. Read `metrics` first; the picture shows *why* they came out that way. Two \
                kinds. `waveform` (default): amplitude against time, one lane per channel, every \
                column drawn as the min/max of the samples behind it so no short pulse is hidden \
                by decimation — use it to see where in a capture something happens, then narrow \
                with `t0_s`/`t1_s`, or `sample0`/`sample1` to zoom straight onto a decoded \
                transaction's `start_sample`/`end_sample`. `eye`: every period of ONE channel folded onto the same axes \
                and shaded by how often traces land there, which turns thousands of edges into \
                one picture of how repeatable the signal is — jitter, overshoot, a slow edge and \
                two populations all become visible where a single trace shows none of them. The \
                eye reports `unit_interval`, `level_low`/`level_high`, `eye_height` (how much \
                room a threshold has), `eye_width`, `duty`, and two jitter numbers that are not \
                the same thing: `jitter_tie_pp` (drift from a best-fit period grid, what closes \
                an eye sideways) and `jitter_cycle_to_cycle_pp`. Check `kind` in the metrics: a \
                repeating signal like a clock folds to `persistence` (every trace agrees, so \
                `eye_width` is the shorter settled stretch), random data folds to a true `data` \
                eye, and a signal that never sits at two levels (a sine) folds as `periodic`, with \
                no eye height, width or threshold — only its period and jitter. `period_s` forces the fold interval; omit it to recover it from the signal. \
                `histogram`: how long the intervals on ONE channel are, as a distribution on a \
                log axis, with the populations they fall into named — the fastest way to diagnose \
                a digital line, because `population_1_length 8.68 us, share 97 %` next to \
                `population_2_length 4.34 us, share 3 %` is a complete finding (a line running at \
                half the expected rate) in two numbers where a picture of 4000 edges is not. \
                Reports `populations`, `dominant_rate`, per-population length/share/count/spread, \
                and `glitches` (intervals two samples or shorter, which say more about the \
                sampling or a floating input than about the signal). `anomalies`: the same \
                intervals, but only the ones that do NOT fit — `outliers` out of `intervals`, the \
                `worst_deviation`, and `findings` naming WHERE the worst are, each with `at_s`. \
                That is the point of it: hand a timestamp straight back as `t0_s`/`t1_s` of a \
                `waveform` call and look at what happened, instead of paging through seconds of \
                samples. On NRZ data (UART, CAN: runs of 1, 2, 3… bit times) each interval is \
                held against the nearest whole number of unit intervals, so a run of several \
                bits is no anomaly; `reference_kind` says which was used, and idle gaps over 16 \
                units are not judged. Both take `interval_kind` (default `period`) and `anomalies` takes \
                `tolerance` (default 0.05). The deviation-against-time picture also tells you \
                which KIND of problem it is: one spike is a stutter, a slope is a clock drifting, \
                a band is jitter — and the count alone distinguishes none of them. \
                `notes` lists anything that stops the picture being read at face value. It \
                refuses rather than drawing nothing: a constant channel, a period too short to \
                fold, more than one channel for anything but a waveform. `include_svg` (default false) returns the \
                document as text in the `svg` field INSTEAD of the image block (never both), for \
                saving to a file — leave it off to get the picture as an image. `format` \
                (default \"svg\") picks the image block's type: \"png\" rasterizes the same \
                picture, for a client that cannot display SVG (one that saves an SVG image \
                block to a file and shows nothing). Every result carries `url`: the same \
                picture at this server's `GET /api/render` (same window, size and format), to \
                save straight into a report with `curl -o fig.svg \"<url>\"` (send the bearer \
                token if the server has one). Example: \
                {\"capture\":\"capture://...\",\"kind\":\"eye\",\"channels\":[0]} -> \
                {\"metrics\":[{\"name\":\"frequency\",\"value\":1000.0,\"unit\":\"Hz\",\
                \"text\":\"1.00 kHz\"},{\"name\":\"eye_height\",\"value\":3.13,\
                \"unit\":\"V\",\"text\":\"3.13 V\"},...]}.",
            input_schema: object_schema(
                vec![
                    ("capture", json!({"type":"string","description":"A capture:// handle from `capture`, `snapshot_recording` or SCPI ACQ:STAR"})),
                    ("kind", json!({"type":"string","enum":["waveform","eye","histogram","anomalies"],"description":"Default \"waveform\""})),
                    ("channels", json!({"type":"array","items":{"type":"integer","minimum":0,"maximum":255},"description":"Hardware channel numbers, as measure and decode take them (D4 is 4, CH1 is 0); omitted draws all. The eye kind takes exactly one"})),
                    ("width", json!({"type":"integer","minimum":240,"maximum":4000,"description":"Pixels, default 900"})),
                    ("height", json!({"type":"integer","minimum":160,"maximum":4000,"description":"Pixels, default 420"})),
                    ("t0_s", json!({"type":"number","minimum":0,"description":"Window start, seconds from the capture's start; default the whole capture"})),
                    ("t1_s", json!({"type":"number","minimum":0,"description":"Window end, seconds; omitted runs to the end of the capture (a note says so)"})),
                    ("span_s", json!({"type":"number","exclusiveMinimum":0,"description":"Window length, seconds: t1_s = t0_s + span_s (t0_s defaults to 0). The natural zoom; not with t1_s"})),
                    ("sample0", json!({"type":"integer","minimum":0,"description":"Window start as a sample number from the capture's start (as `decode`'s start_sample/end_sample); instead of t0_s/t1_s/span_s"})),
                    ("sample1", json!({"type":"integer","minimum":1,"description":"Window end as a sample number; instead of t0_s/t1_s/span_s"})),
                    ("period_s", json!({"type":"number","description":"eye only: the period to fold on. Omit to recover it from the signal"})),
                    ("interval_kind", json!({"type":"string","enum":["high","low","period"],"description":"histogram/anomalies: which intervals to measure. Default \"period\" (is this clock steady?); \"high\"/\"low\" are pulse widths (are these bits the right length?)"})),
                    ("tolerance", json!({"type":"number","minimum":0,"description":"anomalies only: how far an interval may stray from the dominant one before it is called out, as a fraction. Default 0.05 (5 %)"})),
                    ("title", json!({"type":"string","description":"Replaces the generated title on the picture"})),
                    ("include_svg", json!({"type":"boolean","description":"Return the SVG document as text in `svg` instead of as an image block (default false)"})),
                    ("format", json!({"type":"string","enum":["svg","png"],"description":"The image block's type, default \"svg\"; \"png\" for a client that cannot display SVG"})),
                ],
                vec!["capture"],
            ),
        },
        ToolDef {
            name: "list_decoders",
            description: "List the available protocol decoders (`uart`, `i2c`, `spi`, `can`) for `decode`: \
                each one's required channel roles and its options with their types, defaults and \
                allowed values — so a `decode` call can be built without any hardcoded protocol \
                knowledge. Takes no arguments.",
            input_schema: object_schema(vec![], vec![]),
        },
        ToolDef {
            name: "start_recording",
            description: "Start a continuous, rolling recording on a device — unlike `capture`, \
                this never stops on its own; it records until `stop_recording`, until the device is \
                lost, or until the server's memory budget forces it to drop the oldest data (recent \
                data is never lost first). One recording per device at a time; `configure_acquisition` \
                / `capture` on the same device are refused with `device_busy` while it runs. Returns \
                a small integer `rec` id used by every other recorder tool. `rate_hz`, `channels` \
                and `ranges_v` are the same as `configure_acquisition`; without `ranges_v` an \
                analog recording runs at the device's default range. Unknown arguments are \
                refused. Example: {\"device\":\"3-3.4.1.1\",\
                \"rate_hz\":1000000,\"channels\":[0]} then search or wait for patterns on channel D0 \
                while it runs.",
            input_schema: object_schema(
                vec![
                    ("device", json!({"type":"string"})),
                    ("rate_hz", json!({"type":"integer","minimum":1})),
                    ("channels", json!({"type":"array","items":{"type":"integer","minimum":0,"maximum":255}})),
                    ("ranges_v", json!({"type":"array","items":{"type":"number"},"description":"Analog full-scale range per channel, volts"})),
                ],
                vec!["device", "rate_hz", "channels"],
            ),
        },
        ToolDef {
            name: "stop_recording",
            description: "Stop recording `rec` (from `start_recording`/`list_recordings`). The \
                recorded data stays queryable (`search_pattern`, `wait_for_pattern`, \
                `snapshot_recording`) until it is evicted or deleted with `delete_recording`. \
                Example: {\"rec\":1}.",
            input_schema: object_schema(vec![("rec", json!({"type":"integer","minimum":0}))], vec!["rec"]),
        },
        ToolDef {
            name: "delete_recording",
            description: "Delete recording `rec` and free its memory against the recorder's \
                budget (`list_recordings` shows `used_bytes` of `budget_mb`). A live recording is \
                stopped first. A stopped recording otherwise keeps its memory until eviction \
                reclaims it; captures already taken with `snapshot_recording` are kept. \
                Example: {\"rec\":1}.",
            input_schema: object_schema(vec![("rec", json!({"type":"integer","minimum":0}))], vec!["rec"]),
        },
        ToolDef {
            name: "list_recordings",
            description: "List every active or recently-stopped recording: id, device, rate, \
                channels, retained time span (as sample indices `first_sample`/`head_sample` — \
                divide by `rate_hz` for seconds), memory used, compression ratio, and `gaps` \
                (sample spans with no data, where the device stalled and was reset and a digital \
                recording resumed after it). Takes no \
                arguments. Use this to find `rec` ids, or to check how much history is still \
                retained before calling `search_pattern` with an explicit `t0_s`.",
            input_schema: object_schema(vec![], vec![]),
        },
        ToolDef {
            name: "search_pattern",
            description: "Search a recording's retained history for a pattern, returning every \
                match's time (seconds from the recording's start) and sample index — never bulk \
                samples. Pattern language (SPEC §2.9.1), channel names match the device \
                (\"D0\"-\"D7\" digital, \"CH1\"/\"CH2\" analog):\n\
                - `D0 f=1k 100010x101` — bit pattern on D0 at 1000 bit/s (SI suffixes k/M; `x` = \
                  don't care; commas optional; case-insensitive). If the recording has exactly one \
                  digital channel the channel name may be omitted.\n\
                - `D2 rising` / `D2 falling` / `D2 edge` — any edge of that kind.\n\
                - `D1 high >2ms <5ms` — a high pulse whose width is in range (either bound optional).\n\
                - `D1 low >=10ms` — a low level held at least that long.\n\
                - `D3..D0 = 1x01` — multi-channel state, MSB first.\n\
                - `CH1 rising 1.2V` / `CH1 falling 1.2V` — analog level crossing.\n\
                - `CH1 >1.5V` / `CH1 <1.5V` — analog above/below a level.\n\
                A pattern naming a channel the recording lacks is refused at parse time with the \
                list of channels it does have; fix the pattern from that message alone. \
                `t0_s`/`t1_s` default to the whole retained span; searching evicted history is an \
                error naming what is still retained. `max_matches` defaults to 16, hard maximum \
                1024. Each match's `t_s` (and `sample`) is where the matched event *began* — a \
                pulse's or level's onset, an edge's own sample — not where the match became \
                certain: a \"D6 high >=5ms\" match points at the rising edge that started the \
                5 ms, which is 5 ms before the match could be confirmed. A match carrying \
                `\"truncated\":true` is the exception: its pulse or level was already under way \
                when `t0_s` cut the window, so its `t_s` is the window start, not the onset — \
                search from an earlier `t0_s` to find the true onset. If the recording is no longer live, the result carries a `note` saying when and why it ended — nothing after that was recorded, so an empty result there does not mean nothing happened. The `note` also names any gap in the range: a span where the device stalled and was reset and the recording resumed after it, which holds the last level repeated, not what the line did. \
                Example: \
                {\"rec\":1,\"pattern\":\"D0 f=1k 100010x101\",\"max_matches\":16} -> \
                {\"matches\":[{\"t_s\":3.214,\"sample\":3214000}],\"truncated\":false,...}.",
            input_schema: object_schema(
                vec![
                    ("rec", json!({"type":"integer","minimum":0})),
                    ("pattern", json!({"type":"string"})),
                    ("t0_s", json!({"type":"number","description":"Default: start of retained span"})),
                    ("t1_s", json!({"type":"number","description":"Default: end of retained span"})),
                    ("max_matches", json!({"type":"integer","minimum":1,"maximum":1024,"description":"Default 16"})),
                ],
                vec!["rec", "pattern"],
            ),
        },
        ToolDef {
            name: "wait_for_pattern",
            description: "Block until a pattern (same language as `search_pattern`) next matches \
                on a live recording, or until `timeout_ms` elapses. Resolves on the first match \
                after this call arrives (or after `from_s`, if given): an event that began \
                before — such as a level already held when the call arrives — does not count, \
                and `t_s` is the matched event's onset. Returns \
                {\"match\":{\"t_s\":...,\"sample\":...}} or {\"timeout\":true} — a timeout is a \
                normal, non-error outcome, not an exception. `timeout_ms` is capped at 600000 \
                (10 minutes). Errors if the recording stops while waiting. Example: \
                {\"rec\":1,\"pattern\":\"D2 rising\",\"timeout_ms\":10000}.",
            input_schema: object_schema(
                vec![
                    ("rec", json!({"type":"integer","minimum":0})),
                    ("pattern", json!({"type":"string"})),
                    ("timeout_ms", json!({"type":"integer","minimum":0,"maximum":600000,"description":"Default: server default, capped at 600000 (10 min)"})),
                    ("from_s", json!({"type":"number","description":"Only consider matches at or after this time"})),
                ],
                vec!["rec", "pattern"],
            ),
        },
        ToolDef {
            name: "snapshot_recording",
            description: "Decompress `[t0_s, t1_s)` seconds of a recording's history into an \
                ordinary capture handle (`capture://...`), capped at 64 Mi samples, which can then \
                be passed to `measure` or `read_capture` exactly like a `capture` result. Use this \
                to get exact measurements or a waveform preview of something a pattern search or \
                wait already found. Example: {\"rec\":1,\"t0_s\":3.204,\"t1_s\":3.214} for a 10 ms \
                window around a match at t=3.214s.",
            input_schema: object_schema(
                vec![
                    ("rec", json!({"type":"integer","minimum":0})),
                    ("t0_s", json!({"type":"number"})),
                    ("t1_s", json!({"type":"number"})),
                ],
                vec!["rec", "t0_s", "t1_s"],
            ),
        },
        ToolDef {
            name: "serial_list",
            description: "List detected serial (UART) ports: a stable `source` id \
                (\"serial:<usb device>:<interface>\"), a human label, the USB device it belongs to, and whether it \
                is currently available to open. Takes no arguments. Use the `source` value with \
                `serial_open`.",
            input_schema: object_schema(vec![], vec![]),
        },
        ToolDef {
            name: "serial_open",
            description: "Open (or join, if already open by someone else — a browser tab and an \
                agent can share one port) a serial session. `source` is a value from `serial_list`. \
                `baud`/`data_bits`/`parity`/`stop_bits` default to 115200/8/none/1 if omitted; with \
                none of them given, an already-open port is joined at whatever it is open at. A port \
                has one frame format: joining it at different settings is refused \
                (`serial_settings_conflict`, naming both). The result's `serial` is the frame \
                format actually in use. Returns a `session` id for \
                `serial_write`/`serial_read`/`serial_expect`. Opening \
                replays up to 64 KiB of the port's existing scrollback into the first `serial_read`. \
                Example: {\"source\":\"<source from serial_list>\",\"baud\":115200}.",
            input_schema: object_schema(
                vec![
                    ("source", json!({"type":"string"})),
                    ("baud", json!({"type":"integer","minimum":1,"description":"Default 115200"})),
                    ("data_bits", json!({"type":"integer","enum":[5,6,7,8],"description":"Default 8"})),
                    ("parity", json!({"type":"string","enum":["none","even","odd"],"description":"Default none"})),
                    ("stop_bits", json!({"type":"string","enum":["one","two"],"description":"Default one"})),
                ],
                vec!["source"],
            ),
        },
        ToolDef {
            name: "serial_write",
            description: "Write text to an open serial session. `data` is sent as-is (no newline \
                added automatically — include \\r or \\n yourself if the device expects a line \
                ending). Example: {\"session\":3,\"data\":\"help\\r\"}.",
            input_schema: object_schema(
                vec![("session", json!({"type":"integer","minimum":0})), ("data", json!({"type":"string"}))],
                vec!["session", "data"],
            ),
        },
        ToolDef {
            name: "serial_read",
            description: "Read whatever bytes have arrived on a serial session since the last \
                `serial_read` (or since it was opened, for the first call), as UTF-8 text (invalid \
                bytes are replaced, not an error). `max_bytes` bounds one call's response, default \
                4096, hard maximum 16384 — this is a bounded per-call read, not the whole session \
                history. `dropped_bytes > 0` means the session's internal buffer filled up before \
                you read it and that many of the *oldest* unread bytes were discarded; \
                `more_available: true` means bytes remain buffered beyond `max_bytes` — call again \
                immediately to drain them. Example: {\"session\":3,\"max_bytes\":4096}.",
            input_schema: object_schema(
                vec![
                    ("session", json!({"type":"integer","minimum":0})),
                    ("max_bytes", json!({"type":"integer","minimum":1,"maximum":16384,"description":"Default 4096, hard maximum 16384"})),
                ],
                vec!["session"],
            ),
        },
        ToolDef {
            name: "serial_close",
            description: "Leave a serial session opened with `serial_open`. The port itself \
                closes once nobody else (a browser tab, another agent) is attached to it. Close \
                what you open when you are done with it: an open port keeps its USB device busy \
                for everyone. Closing a session that is already closed is not an error. Ending \
                the MCP session also closes every serial session it opened. Example: \
                {\"session\":3}.",
            input_schema: object_schema(
                vec![("session", json!({"type":"integer","minimum":0}))],
                vec!["session"],
            ),
        },
        ToolDef {
            name: "serial_expect",
            description: "Wait until a serial session's output matches a regular expression, or \
                until `timeout_ms` elapses — the serial counterpart of `wait_for_pattern`. Matches \
                against everything currently buffered plus anything that arrives while waiting (a \
                later `serial_read` still sees the same bytes; this does not consume them). Returns \
                {\"matched\":true,\"text\":\"<matched substring>\"} or {\"matched\":false} on \
                timeout — a timeout is a normal outcome, not an error. `timeout_ms` defaults to 5000, \
                hard maximum 60000. Example: {\"session\":3,\"pattern\":\"OK\\\\r\\\\n\",\
                \"timeout_ms\":2000} after `serial_write`ing a command that should answer \"OK\".",
            input_schema: object_schema(
                vec![
                    ("session", json!({"type":"integer","minimum":0})),
                    ("pattern", json!({"type":"string","description":"Regular expression (Rust `regex` crate syntax)"})),
                    ("timeout_ms", json!({"type":"integer","minimum":0,"maximum":60000,"description":"Default 5000, hard maximum 60000"})),
                ],
                vec!["session", "pattern"],
            ),
        },
    ]
}

fn object_schema(properties: Vec<(&str, Value)>, required: Vec<&str>) -> Value {
    let props: serde_json::Map<String, Value> = properties
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    json!({
        "type": "object",
        "properties": Value::Object(props),
        "required": required,
        "additionalProperties": false,
    })
}

/// A trigger given without `kind` is an edge trigger (B-34): `edge` is the
/// only kind, the schema's other three fields already say everything, and
/// two of four models that followed the schema were rejected for omitting
/// it. The wire type stays strict — only the MCP path defaults.
/// What a `render_view` call asked for beyond what its result echoes, to
/// rebuild it as a `GET /api/render` link.
struct RenderLink {
    t0_s: Option<f64>,
    t1_s: Option<f64>,
    period_s: Option<f64>,
    interval_kind: Option<awic_proto::IntervalKind>,
    tolerance: Option<f64>,
    title: Option<String>,
    png: bool,
}

impl RenderLink {
    /// F-09: the same picture at `GET /api/render`, for an agent to save to
    /// a file (`curl -o fig.svg`) instead of digging the image block out of
    /// its client. Absolute when the client's request named the server's
    /// host, else the path alone.
    fn url(&self, base: Option<&str>, r: &awic_proto::RenderResult) -> String {
        let mut q = form_urlencoded::Serializer::new(String::new());
        q.append_pair("capture", &r.capture);
        q.append_pair("kind", r.kind.as_str());
        let channels: Vec<String> = r.channels.iter().map(u8::to_string).collect();
        if !channels.is_empty() {
            q.append_pair("channels", &channels.join(","));
        }
        q.append_pair("width", &r.width.to_string());
        q.append_pair("height", &r.height.to_string());
        for (key, value) in [
            ("t0_s", self.t0_s),
            ("t1_s", self.t1_s),
            ("period_s", self.period_s),
            ("tolerance", self.tolerance),
        ] {
            if let Some(v) = value {
                q.append_pair(key, &v.to_string());
            }
        }
        if let Some(kind) = self.interval_kind {
            let kind = match kind {
                awic_proto::IntervalKind::High => "high",
                awic_proto::IntervalKind::Low => "low",
                awic_proto::IntervalKind::Period => "period",
            };
            q.append_pair("interval_kind", kind);
        }
        if let Some(title) = &self.title {
            q.append_pair("title", title);
        }
        if self.png {
            q.append_pair("format", "png");
        }
        format!("{}/api/render?{}", base.unwrap_or(""), q.finish())
    }
}

/// `sample0`/`sample1` (sample numbers from the capture's start, as
/// `decode` reports them) as the `t0_s`/`t1_s` the window takes, or the
/// times given (issue #32: zooming onto a decoded byte took hand
/// arithmetic). Each is nudged a quarter sample inwards, so the server's
/// rounding — `round`, or `floor` at the start and `ceil` at the end — lands
/// on exactly the sample asked for.
fn sample_window(
    host: &McpHost,
    capture: &str,
    times: (Option<f64>, Option<f64>),
    samples: (Option<u64>, Option<u64>),
) -> Result<(Option<f64>, Option<f64>), Value> {
    match (times, samples) {
        (times, (None, None)) => Ok(times),
        ((None, None), (s0, s1)) => {
            if let (Some(a), Some(b)) = (s0, s1) {
                if b <= a {
                    return Err(tool_err(
                        "invalid_params",
                        "`sample1` must be after `sample0`",
                    ));
                }
            }
            let rate = host.capture_rate_hz(capture).map_err(err_to_value)? as f64;
            Ok((
                s0.map(|s| (s as f64 + 0.25) / rate),
                s1.map(|s| (s as f64 - 0.25).max(0.0) / rate),
            ))
        }
        _ => Err(tool_err(
            "invalid_params",
            "give the window in seconds (`t0_s`/`t1_s`/`span_s`) or in samples \
             (`sample0`/`sample1`), not both",
        )),
    }
}

fn with_default_trigger_kind(mut arguments: Value) -> Value {
    if let Some(trigger) = arguments.get_mut("trigger").and_then(Value::as_object_mut) {
        trigger.entry("kind").or_insert_with(|| json!("edge"));
    }
    arguments
}

/// Every tool's `Args` is `deny_unknown_fields`: an argument a tool does not
/// know is refused, never dropped. `start_recording` used to drop `ranges_v`
/// without a word, so every analog recording ran at the default range (B-73),
/// and any other misspelled or unsupported argument would fail the same way.
fn parse_args<T: DeserializeOwned>(arguments: &Value) -> Result<T, Value> {
    serde_json::from_value(arguments.clone())
        .map_err(|err| tool_err("invalid_params", &format!("bad arguments: {err}")))
}

/// Runs one `tools/call`. Always returns a valid MCP tool result (never a
/// JSON-RPC protocol error) — an unknown tool name or bad arguments are
/// reported as `isError: true`, exactly like a tool that ran and failed,
/// per the MCP spec's split between transport errors and tool errors.
pub async fn call_tool(host: &McpHost, name: &str, arguments: Value) -> Value {
    let arguments = if arguments.is_null() {
        json!({})
    } else {
        arguments
    };
    match dispatch(host, name, arguments).await {
        Ok(mut value) => {
            strip_identity_calibration(&mut value);
            // A tool that draws something returns it under `image`, which is
            // lifted out here into an MCP image content block rather than left
            // in the structured result: that is where a client looks for a
            // picture, and it keeps the bytes out of the text block.
            match take_image(&mut value) {
                Some((mime_type, data)) => tool_ok_with_image(value, &mime_type, data),
                None => tool_ok(value),
            }
        }
        Err(err) => err,
    }
}

/// Removes and returns `{"image": {"mime_type": ..., "data": ...}}`.
fn take_image(value: &mut Value) -> Option<(String, String)> {
    let object = value.as_object_mut()?;
    let image = object.get("image")?;
    let mime_type = image.get("mime_type")?.as_str()?.to_string();
    let data = image.get("data")?.as_str()?.to_string();
    object.remove("image");
    Some((mime_type, data))
}

/// Drops a digital channel's identity calibration (`scale: 1, offset: 0,
/// unit: ""`) anywhere in a tool result: it converts codes to themselves, and
/// repeated per channel it was the bulk of a `start_recording` answer (live
/// test B-15). An analog calibration — a real scale, offset and unit — stays.
fn strip_identity_calibration(value: &mut Value) {
    fn is_identity(c: &Value) -> bool {
        c.get("scale") == Some(&json!(1.0))
            && c.get("offset") == Some(&json!(0.0))
            && c.get("unit") == Some(&json!(""))
    }
    match value {
        Value::Object(map) => {
            let drop = match map.get("calibration") {
                Some(Value::Array(entries)) => {
                    !entries.is_empty() && entries.iter().all(is_identity)
                }
                Some(one) => is_identity(one),
                None => false,
            };
            if drop {
                map.remove("calibration");
            }
            for (_, v) in map.iter_mut() {
                strip_identity_calibration(v);
            }
        }
        Value::Array(entries) => entries.iter_mut().for_each(strip_identity_calibration),
        _ => {}
    }
}

async fn dispatch(host: &McpHost, name: &str, arguments: Value) -> Result<Value, Value> {
    match name {
        "list_instruments" => {
            let r = host.list_instruments().await.map_err(err_to_value)?;
            Ok(serde_json::to_value(r).unwrap_or_default())
        }
        "usb_tree" => {
            let r = host.usb_tree().await.map_err(err_to_value)?;
            Ok(serde_json::to_value(r).unwrap_or_default())
        }
        "set_usb_power" => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                hub: String,
                port: u8,
                on: bool,
            }
            let a: Args = parse_args(&arguments)?;
            let r = host
                .set_usb_power(a.hub, a.port, a.on)
                .await
                .map_err(err_to_value)?;
            Ok(serde_json::to_value(r).unwrap_or_default())
        }
        "set_usb_port_protected" => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                hub: String,
                port: u8,
                mode: String,
            }
            let a: Args = parse_args(&arguments)?;
            let protected = match a.mode.as_str() {
                "lock" => Some(true),
                "unlock" => Some(false),
                "auto" => None,
                other => {
                    return Err(tool_err(
                        "invalid_params",
                        &format!("mode must be \"lock\", \"unlock\" or \"auto\", got {other:?}"),
                    ))
                }
            };
            let r = host
                .set_usb_port_protected(a.hub, a.port, protected)
                .await
                .map_err(err_to_value)?;
            Ok(serde_json::to_value(r).unwrap_or_default())
        }
        "configure_acquisition" => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                device: String,
                rate_hz: u64,
                channels: Vec<u8>,
                samples: u64,
                #[serde(default)]
                trigger: Option<Trigger>,
                #[serde(default)]
                ranges_v: Vec<f32>,
            }
            let arguments = with_default_trigger_kind(arguments);
            let a: Args = parse_args(&arguments)?;
            let r = host
                .configure_acquisition(
                    a.device, a.rate_hz, a.channels, a.samples, a.trigger, a.ranges_v,
                )
                .await
                .map_err(err_to_value)?;
            Ok(serde_json::to_value(r).unwrap_or_default())
        }
        "capture" => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                device: String,
                #[serde(default)]
                timeout_s: Option<f64>,
            }
            let a: Args = parse_args(&arguments)?;
            let timeout_s = match a.timeout_s {
                Some(t) => t,
                None => host.estimated_capture_timeout(&a.device).await,
            };
            let summary = host
                .capture(a.device, timeout_s)
                .await
                .map_err(err_to_value)?;
            Ok(json!({
                "capture": summary.capture,
                "device": summary.device,
                "rate_hz": summary.rate_hz,
                "channels": summary.channels.into_iter().map(|c| match c.constant {
                    // Four "unavailable, the channel never toggles" entries
                    // say less than one word, and the level is the answer
                    // for an idle bus line (live test B-11).
                    Some(level) => json!({
                        "channel": c.channel,
                        "constant": if level { "high" } else { "low" },
                    }),
                    None => json!({
                        "channel": c.channel,
                        "results": c.results,
                    }),
                }).collect::<Vec<_>>(),
                "triggered": summary.triggered_at.is_some(),
                "trigger_sample": summary.triggered_at.as_ref().map(|t| t.sample),
                "trigger_t_s": summary.triggered_at.as_ref().map(|t| t.t_s),
            }))
        }
        "read_capture" => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                capture: String,
                channel: u8,
                #[serde(default)]
                max_points: Option<u32>,
                #[serde(default)]
                t0_s: Option<f64>,
                #[serde(default)]
                t1_s: Option<f64>,
                #[serde(default)]
                sample0: Option<u64>,
                #[serde(default)]
                sample1: Option<u64>,
            }
            let a: Args = parse_args(&arguments)?;
            let (t0_s, t1_s) =
                sample_window(host, &a.capture, (a.t0_s, a.t1_s), (a.sample0, a.sample1))?;
            let r = host
                .read_capture(a.capture, a.channel, a.max_points, t0_s, t1_s)
                .await
                .map_err(err_to_value)?;
            Ok(serde_json::to_value(r).unwrap_or_default())
        }
        "measure" => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                capture: String,
                channel: u8,
                measurements: Vec<MeasurementKind>,
            }
            let a: Args = parse_args(&arguments)?;
            let r = host
                .measure(a.capture, a.channel, a.measurements)
                .await
                .map_err(err_to_value)?;
            Ok(json!({ "results": r }))
        }
        "decode" => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                #[serde(default)]
                capture: Option<String>,
                #[serde(default)]
                rec: Option<u16>,
                #[serde(default)]
                t0_s: Option<f64>,
                #[serde(default)]
                t1_s: Option<f64>,
                decoder: awic_proto::DecoderId,
                channels: Vec<awic_proto::ChannelMapping>,
                #[serde(default)]
                options: Vec<awic_proto::OptionSetting>,
                #[serde(default)]
                max_annotations: Option<u32>,
                #[serde(default)]
                max_level: Option<u8>,
                #[serde(default)]
                format: Option<awic_proto::DecodeFormat>,
                #[serde(default)]
                max_bytes: Option<u32>,
                #[serde(default)]
                fold: Option<bool>,
            }
            let a: Args = parse_args(&arguments)?;
            host.decode(
                a.capture,
                a.rec,
                a.t0_s,
                a.t1_s,
                a.decoder,
                a.channels,
                a.options,
                a.max_annotations,
                // Bit-level annotations are three quarters of a decode and
                // are rarely what an agent is reading for (live test F-02);
                // the graph asks for all of them explicitly.
                Some(a.max_level.unwrap_or(awic_decode::LEVEL_FIELD)),
                a.format,
                a.max_bytes,
                a.fold,
            )
            .await
            .map_err(err_to_value)
        }
        "survey_channels" => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                #[serde(default)]
                device: Option<String>,
                #[serde(default)]
                capture: Option<String>,
                #[serde(default)]
                rec: Option<u16>,
                #[serde(default)]
                t0_s: Option<f64>,
                #[serde(default)]
                t1_s: Option<f64>,
                #[serde(default)]
                duration_s: Option<f64>,
                #[serde(default)]
                rate_hz: Option<u64>,
            }
            let a: Args = parse_args(&arguments)?;
            let given = u8::from(a.device.is_some())
                + u8::from(a.capture.is_some())
                + u8::from(a.rec.is_some());
            if given != 1 {
                return Err(tool_err(
                    "invalid_params",
                    "give exactly one of `device` (survey a fresh capture), `capture` or `rec`",
                ));
            }
            let capture = match a.device {
                None => a.capture,
                Some(device) => {
                    // Take the capture ourselves, so one call answers the
                    // question on a board nobody has configured yet.
                    let instruments = host.list_instruments().await.map_err(err_to_value)?;
                    let found = instruments.devices.iter().find(|d| d.id == device);
                    let Some(found) = found else {
                        return Err(tool_err(
                            "device_not_found",
                            &format!("no instrument {device:?}; call list_instruments"),
                        ));
                    };
                    let digital = found.capabilities.iter().find_map(|c| match c {
                        awic_proto::CapabilityInfo::Digital {
                            channels,
                            max_rate_hz,
                            ..
                        } => Some((*channels, *max_rate_hz)),
                        _ => None,
                    });
                    let Some((channels, max_rate_hz)) = digital else {
                        return Err(tool_err(
                            "not_digital",
                            &format!(
                                "{device:?} has no digital channels; survey_channels reads logic \
                                 lines. For an analog capture, `capture` already reports \
                                 Vpp/frequency/duty per channel"
                            ),
                        ));
                    };
                    let rate_hz = a.rate_hz.unwrap_or(4_000_000).min(max_rate_hz.max(1));
                    let duration_s = a.duration_s.unwrap_or(1.0).clamp(0.001, 10.0);
                    let samples = ((rate_hz as f64 * duration_s) as u64).clamp(1_000, 64_000_000);
                    host.configure_acquisition(
                        device.clone(),
                        rate_hz,
                        (0..channels).collect(),
                        samples,
                        None,
                        vec![],
                    )
                    .await
                    .map_err(err_to_value)?;
                    let timeout_s = (duration_s * 2.0 + 5.0).min(60.0);
                    let summary = host
                        .capture(device, timeout_s)
                        .await
                        .map_err(err_to_value)?;
                    Some(summary.capture)
                }
            };
            let r = host
                .survey(capture, a.rec, a.t0_s, a.t1_s)
                .await
                .map_err(err_to_value)?;
            Ok(serde_json::to_value(r).unwrap_or_default())
        }
        "render_view" => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                capture: String,
                #[serde(default)]
                kind: Option<String>,
                #[serde(default)]
                channels: Vec<u8>,
                #[serde(default)]
                width: Option<u32>,
                #[serde(default)]
                height: Option<u32>,
                #[serde(default)]
                t0_s: Option<f64>,
                #[serde(default)]
                t1_s: Option<f64>,
                #[serde(default)]
                span_s: Option<f64>,
                #[serde(default)]
                sample0: Option<u64>,
                #[serde(default)]
                sample1: Option<u64>,
                #[serde(default)]
                period_s: Option<f64>,
                #[serde(default)]
                interval_kind: Option<String>,
                #[serde(default)]
                tolerance: Option<f64>,
                #[serde(default)]
                title: Option<String>,
                #[serde(default)]
                include_svg: bool,
                #[serde(default)]
                format: Option<String>,
            }
            let a: Args = parse_args(&arguments)?;
            let png =
                match a.format.as_deref() {
                    None | Some("svg") => false,
                    Some("png") if a.include_svg => return Err(tool_err(
                        "invalid_params",
                        "`include_svg` returns the SVG document as text; not with `format` \"png\"",
                    )),
                    Some("png") => true,
                    Some(other) => {
                        return Err(tool_err(
                            "invalid_params",
                            &format!("no image format {other:?}; use svg or png"),
                        ))
                    }
                };
            let kind = match a.kind.as_deref() {
                None | Some("waveform") => awic_proto::RenderKind::Waveform,
                Some("eye") => awic_proto::RenderKind::Eye,
                Some("histogram") => awic_proto::RenderKind::Histogram,
                Some("anomalies") => awic_proto::RenderKind::Anomalies,
                Some(other) => {
                    return Err(tool_err(
                        "invalid_params",
                        &format!(
                            "no view kind {other:?}; use waveform, eye, histogram or anomalies"
                        ),
                    ))
                }
            };
            let interval_kind = match a.interval_kind.as_deref() {
                None => None,
                Some("high") => Some(awic_proto::IntervalKind::High),
                Some("low") => Some(awic_proto::IntervalKind::Low),
                Some("period") => Some(awic_proto::IntervalKind::Period),
                Some(other) => {
                    return Err(tool_err(
                        "invalid_params",
                        &format!("no interval kind {other:?}; use high, low or period"),
                    ))
                }
            };
            // `span_s` is the natural zoom: t0_s + span_s (B-36).
            let t1_s = match (a.span_s, a.t1_s) {
                (None, t1) => t1,
                (Some(_), Some(_)) => {
                    return Err(tool_err(
                        "invalid_params",
                        "give `t1_s` or `span_s`, not both",
                    ))
                }
                (Some(span), None) if span > 0.0 => Some(a.t0_s.unwrap_or(0.0) + span),
                (Some(_), None) => {
                    return Err(tool_err("invalid_params", "`span_s` must be positive"))
                }
            };
            let t0_s = a.t0_s.or(a.span_s.map(|_| 0.0));
            let (t0_s, t1_s) =
                sample_window(host, &a.capture, (t0_s, t1_s), (a.sample0, a.sample1))?;
            let link = RenderLink {
                t0_s,
                t1_s,
                period_s: a.period_s,
                interval_kind,
                tolerance: a.tolerance,
                title: a.title.clone(),
                png,
            };
            let r = host
                .render(
                    a.capture,
                    kind,
                    a.channels,
                    a.width,
                    a.height,
                    t0_s,
                    t1_s,
                    a.period_s,
                    interval_kind,
                    a.tolerance,
                    a.title,
                )
                .await
                .map_err(err_to_value)?;
            use base64::Engine;
            let mut value = json!({
                "capture": r.capture,
                "url": link.url(host.base_url(), &r),
                "kind": r.kind.as_str(),
                "width": r.width,
                "height": r.height,
                "channels": r.channels,
                "metrics": r.metrics,
                "findings": r.findings,
                "notes": r.notes,
                "bytes": r.svg.len(),
            });
            // The document goes out once (B-42): as the image block by
            // default, or as the `svg` text field when asked for — never
            // both, which put a 100 KB+ picture into the result twice.
            if a.include_svg {
                value["svg"] = json!(r.svg);
            } else if png {
                // F-09: the same picture as PNG, for clients that cannot show
                // SVG.
                let png = awic_render::png::svg_to_png(&r.svg)
                    .map_err(|e| tool_err("render_failed", &e))?;
                value["bytes"] = json!(png.bytes.len());
                if let Some(note) = png.note {
                    if let Some(notes) = value["notes"].as_array_mut() {
                        notes.push(json!(note));
                    }
                }
                let encoded = base64::engine::general_purpose::STANDARD.encode(&png.bytes);
                value["image"] = json!({ "mime_type": "image/png", "data": encoded });
            } else {
                let encoded = base64::engine::general_purpose::STANDARD.encode(r.svg.as_bytes());
                value["image"] = json!({ "mime_type": r.mime_type, "data": encoded });
            }
            Ok(value)
        }
        "power_cycle_usb_port" => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                hub: String,
                port: u8,
                #[serde(default)]
                off_ms: Option<u32>,
                #[serde(default)]
                wait_s: Option<f64>,
            }
            let a: Args = parse_args(&arguments)?;
            let r = host
                .usb_cycle(a.hub, a.port, a.off_ms, a.wait_s)
                .await
                .map_err(err_to_value)?;
            let mut value = serde_json::to_value(r).unwrap_or_default();
            // A port that comes back empty is ambiguous: nothing plugged in,
            // or power cut where software cannot reach it (B-50).
            if let Some(obj) = value.as_object_mut() {
                if obj.get("device").is_none_or(Value::is_null) {
                    obj.insert("note".into(), json!(NOTHING_ENUMERATED));
                }
            }
            Ok(value)
        }
        "label_usb_port" => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                hub: String,
                port: u8,
                #[serde(default)]
                label: Option<String>,
                #[serde(default)]
                dut: Option<bool>,
            }
            let a: Args = parse_args(&arguments)?;
            let r = host
                .usb_label(a.hub, a.port, a.label, a.dut)
                .await
                .map_err(err_to_value)?;
            Ok(serde_json::to_value(r).unwrap_or_default())
        }
        "list_decoders" => {
            let r = host.decode_list().await.map_err(err_to_value)?;
            Ok(serde_json::to_value(r).unwrap_or_default())
        }
        "start_recording" => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                device: String,
                rate_hz: u64,
                channels: Vec<u8>,
                #[serde(default)]
                ranges_v: Vec<f32>,
            }
            let a: Args = parse_args(&arguments)?;
            let r = host
                .start_recording(a.device, a.rate_hz, a.channels, a.ranges_v)
                .await
                .map_err(err_to_value)?;
            Ok(serde_json::to_value(r).unwrap_or_default())
        }
        "stop_recording" => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                rec: u16,
            }
            let a: Args = parse_args(&arguments)?;
            host.stop_recording(a.rec).await.map_err(err_to_value)?;
            Ok(json!({ "rec": a.rec, "stopped": true }))
        }
        "delete_recording" => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                rec: u16,
            }
            let a: Args = parse_args(&arguments)?;
            host.delete_recording(a.rec).await.map_err(err_to_value)?;
            Ok(json!({ "rec": a.rec, "deleted": true }))
        }
        "list_recordings" => {
            let r = host.list_recordings().await.map_err(err_to_value)?;
            Ok(serde_json::to_value(r).unwrap_or_default())
        }
        "search_pattern" => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                rec: u16,
                pattern: String,
                #[serde(default)]
                t0_s: Option<f64>,
                #[serde(default)]
                t1_s: Option<f64>,
                #[serde(default)]
                max_matches: Option<u32>,
            }
            let a: Args = parse_args(&arguments)?;
            let r = host
                .search_pattern(a.rec, a.pattern, a.t0_s, a.t1_s, a.max_matches)
                .await
                .map_err(err_to_value)?;
            Ok(serde_json::to_value(r).unwrap_or_default())
        }
        "wait_for_pattern" => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                rec: u16,
                pattern: String,
                #[serde(default)]
                timeout_ms: Option<u32>,
                #[serde(default)]
                from_s: Option<f64>,
            }
            let a: Args = parse_args(&arguments)?;
            let r = host
                .wait_for_pattern(a.rec, a.pattern, a.timeout_ms, a.from_s)
                .await
                .map_err(err_to_value)?;
            Ok(serde_json::to_value(r).unwrap_or_default())
        }
        "snapshot_recording" => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                rec: u16,
                t0_s: f64,
                t1_s: f64,
            }
            let a: Args = parse_args(&arguments)?;
            let r = host
                .snapshot_recording(a.rec, a.t0_s, a.t1_s)
                .await
                .map_err(err_to_value)?;
            Ok(serde_json::to_value(r).unwrap_or_default())
        }
        "serial_list" => {
            let r = host.serial_list().await.map_err(err_to_value)?;
            Ok(json!({ "sources": r }))
        }
        "serial_open" => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                source: String,
                #[serde(default)]
                baud: Option<u32>,
                #[serde(default)]
                data_bits: Option<u8>,
                #[serde(default)]
                parity: Option<Parity>,
                #[serde(default)]
                stop_bits: Option<StopBits>,
            }
            let a: Args = parse_args(&arguments)?;
            // Nothing given: join the port at whatever it is open at (a
            // browser tab's 9600, say), or open it at 115200 8N1 if it is not
            // open yet. Anything given: those settings, filled out with the
            // defaults — and a join at other settings is refused.
            let settings = (a.baud.is_some()
                || a.data_bits.is_some()
                || a.parity.is_some()
                || a.stop_bits.is_some())
            .then(|| SerialSettings {
                baud: a.baud.unwrap_or(115_200),
                data_bits: a.data_bits.unwrap_or(8),
                parity: a.parity.unwrap_or(Parity::None),
                stop_bits: a.stop_bits.unwrap_or(StopBits::One),
            });
            let r = host
                .serial_open(a.source, settings)
                .await
                .map_err(err_to_value)?;
            Ok(serde_json::to_value(r).unwrap_or_default())
        }
        "serial_write" => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                session: u16,
                data: String,
            }
            let a: Args = parse_args(&arguments)?;
            host.serial_write(a.session, a.data.into_bytes())
                .await
                .map_err(err_to_value)?;
            Ok(json!({ "written": true }))
        }
        "serial_read" => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                session: u16,
                #[serde(default)]
                max_bytes: Option<u32>,
            }
            let a: Args = parse_args(&arguments)?;
            let max_bytes = a.max_bytes.unwrap_or(4096).clamp(1, 16_384) as usize;
            let r = host
                .serial_read(a.session, max_bytes)
                .await
                .map_err(err_to_value)?;
            Ok(json!({
                "data": String::from_utf8_lossy(&r.data),
                "bytes_read": r.data.len(),
                "dropped_bytes": r.dropped,
                "more_available": r.more_available,
            }))
        }
        "serial_close" => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                session: u16,
            }
            let a: Args = parse_args(&arguments)?;
            host.serial_close(a.session).await.map_err(err_to_value)?;
            Ok(json!({ "session": a.session, "closed": true }))
        }
        "serial_expect" => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                session: u16,
                pattern: String,
                #[serde(default)]
                timeout_ms: Option<u32>,
            }
            let a: Args = parse_args(&arguments)?;
            let re = regex::Regex::new(&a.pattern).map_err(|err| {
                tool_err(
                    "invalid_pattern",
                    &format!("invalid regex {:?}: {err}", a.pattern),
                )
            })?;
            let timeout_ms = a.timeout_ms.unwrap_or(5_000).clamp(0, 60_000) as u64;
            match host
                .serial_expect(a.session, &re, timeout_ms)
                .await
                .map_err(err_to_value)?
            {
                Some(text) => Ok(json!({ "matched": true, "text": text })),
                None => Ok(json!({ "matched": false })),
            }
        }
        other => Err(tool_err(
            "unknown_tool",
            &format!("no such tool {other:?}; call tools/list for the current catalog"),
        )),
    }
}

fn err_to_value(err: super::host::McpToolError) -> Value {
    tool_err(&err.code, &err.msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_has_a_valid_object_schema() {
        for tool in tool_defs() {
            assert_eq!(tool.input_schema["type"], "object", "{}", tool.name);
            assert!(tool.input_schema["properties"].is_object(), "{}", tool.name);
            assert!(!tool.description.is_empty(), "{}", tool.name);
        }
    }

    #[test]
    fn identity_calibration_is_stripped_but_a_real_one_is_kept() {
        let mut v = json!({
            "rec": 1,
            "channels": [{"channel": 0, "calibration": {"channel": 0, "scale": 1.0, "offset": 0.0, "unit": ""}}],
            "analog": {"calibration": [{"channel": 0, "scale": 0.04, "offset": -5.0, "unit": "V"}]},
        });
        strip_identity_calibration(&mut v);
        assert_eq!(v["channels"][0].get("calibration"), None);
        assert!(v["analog"]["calibration"].is_array(), "{v}");
    }

    #[test]
    fn a_trigger_without_kind_is_an_edge_trigger() {
        // B-34: the schema once listed no `kind` and the server demanded one.
        let args = json!({"trigger": {"channel": 0, "slope": "rising", "level": 1.5}});
        assert!(serde_json::from_value::<awic_proto::Trigger>(args["trigger"].clone()).is_err());
        let fixed = with_default_trigger_kind(args);
        let t: awic_proto::Trigger = serde_json::from_value(fixed["trigger"].clone()).unwrap();
        assert!(matches!(t, awic_proto::Trigger::Edge { channel: 0, .. }));
        // No trigger stays no trigger.
        assert_eq!(with_default_trigger_kind(json!({"a":1})), json!({"a":1}));
        let defs = tool_defs();
        let cfg = defs
            .iter()
            .find(|t| t.name == "configure_acquisition")
            .unwrap();
        let required = cfg.input_schema["properties"]["trigger"]["required"].to_string();
        assert!(!required.contains("kind"), "{required}");
        // B-96: a digital trigger has no level to give.
        assert!(!required.contains("level"), "{required}");
        let digital =
            with_default_trigger_kind(json!({"trigger": {"channel": 4, "slope": "falling"}}));
        let t: awic_proto::Trigger = serde_json::from_value(digital["trigger"].clone()).unwrap();
        assert!(matches!(t, awic_proto::Trigger::Edge { level: None, .. }));
    }

    #[test]
    fn descriptions_document_the_read_window_span_and_store_bounds() {
        // B-35, B-36, B-37.
        let defs = tool_defs();
        let read = defs.iter().find(|t| t.name == "read_capture").unwrap();
        for k in ["t0_s", "t1_s"] {
            assert!(read.input_schema["properties"][k].is_object(), "{k}");
            assert!(read.description.contains(k), "{k}");
        }
        let cap = defs.iter().find(|t| t.name == "capture").unwrap();
        let (n, bytes) = (
            awic_core::capture_store::DEFAULT_MAX_CAPTURES,
            awic_core::capture_store::DEFAULT_MAX_BYTES / (1024 * 1024),
        );
        assert!(cap
            .description
            .contains(&format!("{n} captures / {bytes} MB")));
        let render = defs.iter().find(|t| t.name == "render_view").unwrap();
        assert!(render.input_schema["properties"]["span_s"].is_object());
    }

    #[test]
    fn tool_names_are_unique() {
        let defs = tool_defs();
        let mut names: Vec<&str> = defs.iter().map(|t| t.name).collect();
        names.sort_unstable();
        let mut dedup = names.clone();
        dedup.dedup();
        assert_eq!(names, dedup, "duplicate tool name");
    }
}
