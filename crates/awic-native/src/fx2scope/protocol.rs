//! The `fx2scope` wire protocol for the FX2 USB oscilloscope (SPECIFICATION.md
//! §2.5), a 2-channel analog scope on the same Cypress FX2 platform as
//! `fx2lafw` — programmed with a *different* firmware image
//! (`fx2lafw-hantek-6022bl.fw`) that exposes a different command set on
//! endpoint 0.
//!
//! # Ground truth, not documentation (SPECIFICATION.md §2.5)
//!
//! Every constant and formula in this module was established by running
//! `sigrok-cli --driver hantek-6xxx` (loglevel 5) against the real device on
//! this bench and reading its own protocol trace — `scope_6xxx_write_control:
//! <request> <value>` lines the driver itself prints for every control
//! transfer it issues — cross-checked against `libsigrok`'s own
//! `src/hardware/hantek-6xxx/{protocol.h,protocol.c,api.c}` (GPL, read only
//! for these facts, never copied — see this crate's top-level licensing
//! note). `usbmon` was unavailable on this host (kernel lockdown blocks
//! `debugfs`, confirmed via `dmesg`: "Lockdown: cat: debugfs access is
//! restricted"), so the driver's own request/value trace plus the reference
//! source stood in as the ground truth this module encodes. Facts below are
//! marked **verified** (reproduced on the bench) or **from source** (read in
//! `libsigrok`, not independently exercised here).
//!
//! **Firmware file**: `fx2lafw-hantek-6022bl.fw` — confirmed by running
//! `sigrok-cli --driver hantek-6xxx -l 5 --scan` and reading the `ezusb:
//! Uploading firmware 'fx2lafw-hantek-6022bl.fw'` log line. **Verified.**
//!
//! **Post-firmware VID:PID is `1d50:608e` ("OpenMoko, Inc. Fx2lafw"), *not*
//! `04b5:602a`** as an early draft of the specification assumed from
//! documentation. `lsusb -v` on the re-enumerated device on this bench shows
//! `idVendor 0x1d50`, `idProduct 0x608e`, `iManufacturer "sigrok"`,
//! `iProduct "fx2lafw"`, `iSerial "Hantek 6022BL"`. This is the *generic*
//! sigrok fx2lafw USB identity — the firmware image does not carry a
//! The oscilloscope-specific PID at all; only the serial-number string names the
//! model. **Verified**, and recorded here as the load-bearing correction
//! this module's [`super::backend`] acts on.
//!
//! **Bulk endpoint**: `0x86`, interface 0, **alternate setting 0** (the
//! interface also exposes alt settings 1-7, all isochronous on `0x82` —
//! unused by this driver, presumably for other fx2lafw-firmware devices in
//! the same product family that stream isochronously). Confirmed via
//! `lsusb -v -d 1d50:608e`. **Verified.**
//!
//! **Command set** (all vendor, device-recipient control OUT, `wIndex 0`;
//! `libsigrok`'s `enum control_requests`, cross-checked against every write
//! this driver traced):
//!
//! | Request | Name | Value written | Verified how |
//! |---|---|---|---|
//! | `0xE0` | CH1 range | [`RangeCode`] | every `vdiv=`, both channels |
//! | `0xE1` | CH2 range | [`RangeCode`] | every `vdiv=`, both channels |
//! | `0xE2` | sample rate | [`SAMPLE_RATE_REGS`] | every rate in the ladder |
//! | `0xE3` | start (`1`) / stop (`0`) | `1` or `0` | every capture |
//! | `0xE4` | channel count | *(from source only — never sent, see below)* | — |
//! | `0xE5` | coupling | *(from source only — never sent for this model)* | — |
//!
//! **`0xE4` (channel count) is dead for this device model.** SPECIFICATION.md
//! §2.5 lists it as "active channel count (1 or 2)", following
//! `libsigrok`'s `scope_6xxx_update_channels()`. But tracing an explicit
//! `-C CH1` (one channel) capture against `-C CH1 -C CH2` (both) at the same
//! rate and sample count shows **identical bytes fetched from the device**
//! (4096 for a 2000-sample request, either way) and **no `0xE4` write in
//! either trace**. The device always streams both channels interleaved
//! (2 bytes per timepoint) regardless of what the host asked for; "1
//! channel" is a **host-side selection of which byte to keep**, not a
//! hardware bandwidth reduction. This driver therefore never writes `0xE4`
//! and never asks the device for less than both channels' worth of bytes —
//! see [`super::driver`]. **Verified** (byte-count comparison on real
//! hardware).
//!
//! **`0xE5` (coupling) is present in `libsigrok`'s enum but the CLI reports
//! "coupling not supported" for this device** (`--show -g CH1` lists only
//! `coupling: DC (current)`, non-configurable) and no `0xE5` write appears in
//! any trace. Not used by this driver. **Verified** (absence in every
//! trace) — noted here because SPECIFICATION.md's protocol table guessed
//! `0xE5`/`0xE6` were *calibration offset* registers; ground truth says
//! `0xE5` is coupling (unused) and **there is no `0xE6` in this protocol at
//! all** — `libsigrok`'s enum stops at `COUPLING_REG = 0xE5`. See the module
//! doc on calibration below.
//!
//! ## Calibration: no on-device offsets in this protocol
//!
//! SPECIFICATION.md §2.3.1/§2.5 anticipated per-range calibration offsets
//! "the device stores" and a driver would read back. **Ground truth found
//! none**: no control-transfer *read* of any kind appears in any trace
//! (`sigrok-cli -l 5` logs every `scope_6xxx_write_control` call but never a
//! read), and `libsigrok`'s own `struct dev_context` (read via
//! `protocol.h`) carries no calibration/offset field — only `voltage[2]` (the
//! selected range index per channel) and the `vdivs` range table itself.
//! `libsigrok`'s conversion from raw byte to volts (`send_chunk()` in
//! `api.c`, read for this fact only) is a pure function of the selected
//! range, no device-read calibration term:
//!
//! ```text
//! RANGE = 10 * vdiv_volts               // "500 mV/div represents 5 V
//!                                        // peak-to-peak" -- 10 divisions
//! volts = raw * (RANGE / 255) - RANGE / 2
//! ```
//!
//! which is exactly [`ChannelCalibration`]'s `raw * scale + offset` with
//! `scale = RANGE / 255` and `offset = -RANGE / 2`. This module's
//! [`calibration_for`] reproduces that formula (derived independently, not
//! copied) rather than inventing a different one, since it is the same
//! oracle SPECIFICATION.md §14 names for cross-checking native drivers.
//! **From source** (the arithmetic itself; not independently exercised
//! against a known voltage on this bench — see the crate's hardware test
//! doc comment for exactly what a real signal would additionally confirm).
//!
//! Note this is **± full scale = `RANGE / 2` = `5 * vdiv_volts`**, not the
//! vdiv (volts/division) figure itself and not peak-to-peak — the exact
//! distinction SPECIFICATION.md §2.3.1 warns an earlier client got wrong by
//! treating a ± range as peak-to-peak and halving every trace.

use crate::error::{NativeError, Result};

/// `bRequest` values (`libsigrok`'s `enum control_requests`, `protocol.h`).
pub const CMD_VDIV_CH1: u8 = 0xE0;
pub const CMD_VDIV_CH2: u8 = 0xE1;
pub const CMD_SAMPLERATE: u8 = 0xE2;
pub const CMD_TRIGGER: u8 = 0xE3;

/// `USB_INTERFACE`: the interface this firmware's bulk alt setting (0) lives
/// on — see this module's doc comment. No explicit `SET_INTERFACE` is
/// needed: the device enumerates with alt 0 already active (bench finding,
/// see `driver::Fx2ScopeInstrument::prepare`'s doc comment).
pub const INTERFACE_NUMBER: u8 = 0;

/// 2 analog channels, CH1 and CH2 (SPECIFICATION.md's verified ground
/// truth). Always both sampled — see this module's doc comment on `0xE4`.
pub const CHANNEL_COUNT: u8 = 2;

/// The device's fixed sample-rate ladder, descending — matches
/// `sigrok-cli --driver hantek-6xxx --show`'s list on this bench exactly:
/// "48/30/24/16/8 (default)/4/1 MHz; 500/200/100 kHz" (SPECIFICATION.md
/// §2.5's verified ground truth).
pub const SAMPLE_RATES_HZ: &[u64] = &[
    48_000_000, 30_000_000, 24_000_000, 16_000_000, 8_000_000, 4_000_000, 1_000_000, 500_000,
    200_000, 100_000,
];

/// `CMD_SAMPLERATE`'s register value for each ladder rate, same order as
/// [`SAMPLE_RATES_HZ`]. `libsigrok`'s `SAMPLERATE_REGS` macro, verified
/// against this device's own trace for every entry: `8_000_000 Hz -> 0x08`
/// (the device's power-on default), `500_000 -> 0x32`, `200_000 -> 0x14`,
/// `100_000 -> 0x0a`, `48_000_000 -> 0x30`, `24_000_000 -> 0x18`,
/// `1_000_000 -> 0x01`. The pattern (MHz-range rates encode as their MHz
/// value; kHz-range rates encode as one tenth their kHz value) was read from
/// `libsigrok`'s header and independently confirmed for every rate above by
/// tracing `sigrok-cli -c samplerate=<rate>`.
const SAMPLE_RATE_REGS: &[u8] = &[48, 30, 24, 16, 8, 4, 1, 50, 20, 10];

/// Highest rate this driver has *measured* as sustainable over USB 2.0 bulk
/// with **both** channels' bytes always on the wire (every rate costs the
/// same bytes/sec regardless of channel selection — see this module's `0xE4`
/// doc comment above): at `MAX_SUSTAINED_RATE_HZ`, the wire carries
/// `2 * MAX_SUSTAINED_RATE_HZ` bytes/s.
///
/// Measured on this bench against the front-panel CAL square wave, which is
/// an absolute timebase check independent of the driver's own accounting:
///
/// | rate   | delivered   | CAL reads   | per-period extremes |
/// |--------|-------------|-------------|---------------------|
/// | 16 MHz | 16.00 MS/s  | 1000.00 Hz  | -6 / +16 samples (±0.4 us jitter), 7 of 7 captures of 32 MB |
/// | 24 MHz | 22.53 MS/s  | **1065 Hz** | uniform loss: every period short |
/// | 30 MHz | 22.53 MS/s  | 1332 Hz     | |
/// | 48 MHz | 22.53 MS/s  | 2131 Hz     | |
///
/// Above 16 MHz the device delivers ~22.5 MS/s — the bus ceiling, ~45 MB/s —
/// whatever was asked, dropping samples *evenly*: a 24 MHz capture looks
/// perfectly continuous while its timebase is 6.5 % off. 24/30/48 MHz stay in
/// [`SAMPLE_RATES_HZ`] as valid firmware commands, and
/// [`super::driver::Fx2ScopeInstrument::configure`] refuses them.
///
/// (An earlier version of this comment described 16/24 MHz measurements that
/// predated the driver streaming at all, and a failure mode — 0 bytes — that
/// does not occur. The table above is what the hardware does.)
pub const MAX_SUSTAINED_RATE_HZ: u64 = 16_000_000;

/// The rate ladder actually worth offering in the UI (SPECIFICATION.md §5.2):
/// every entry of [`SAMPLE_RATES_HZ`] the device can sustain,
/// i.e. up to [`MAX_SUSTAINED_RATE_HZ`]. [`super::driver::Fx2ScopeInstrument::configure`]
/// refuses the rest, so a UI that let a user pick — or, worse, defaulted to
/// (the "the UI cannot pick 1 MHz for the logic analyzer" sibling bug,
/// caught live on the bench: defaulting to the fastest advertised rate
/// picked an unsustainable 48 MHz and refused to start) — one of them would
/// never actually start. Computed rather than duplicated so it can't drift
/// from either constant.
pub fn sustained_rates_hz() -> Vec<u64> {
    SAMPLE_RATES_HZ
        .iter()
        .copied()
        .filter(|&r| r <= MAX_SUSTAINED_RATE_HZ)
        .collect()
}

pub fn samplerate_reg(rate_hz: u64) -> Result<u8> {
    SAMPLE_RATES_HZ
        .iter()
        .position(|&r| r == rate_hz)
        .map(|i| SAMPLE_RATE_REGS[i])
        .ok_or(NativeError::UnsupportedSampleRate { rate_hz })
}

/// One selectable vertical range: volts/division as libsigrok's `vdiv`
/// config key would show it, the register code [`CMD_VDIV_CH1`]/
/// [`CMD_VDIV_CH2`] takes, and the ± full-scale range this driver reports
/// and calibrates against (`5 * vdiv_v`, per this module's doc comment).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Range {
    pub vdiv_v: f32,
    pub code: u8,
    pub range_v: f32,
}

/// `libsigrok`'s `VDIV_VALUES`/`VDIV_REG` for this device (4 of the 7-entry
/// table; the rest serve other `hantek-6xxx`-family devices), each entry
/// verified against this device's own trace: `vdiv=1V` -> CH1 code `1`,
/// default (500 mV) -> code `2`, `vdiv=250mV` -> code `5`, `vdiv=100mV` ->
/// code `10`, confirmed identically on CH2.
pub const RANGES: &[Range] = &[
    Range {
        vdiv_v: 1.0,
        code: 1,
        range_v: 5.0,
    },
    Range {
        vdiv_v: 0.5,
        code: 2,
        range_v: 2.5,
    },
    Range {
        vdiv_v: 0.25,
        code: 5,
        range_v: 1.25,
    },
    Range {
        vdiv_v: 0.1,
        code: 10,
        range_v: 0.5,
    },
];

/// The device's power-on default range (500 mV/div, code 2) — confirmed as
/// the value `sigrok-cli --show` reports "current" with no prior `vdiv=`
/// config, and the code this driver's own trace wrote when no range was
/// requested.
pub const DEFAULT_RANGE_V: f32 = 2.5;

/// Finds the configured [`Range`] whose ± full-scale value is closest to
/// `requested_v` (SPECIFICATION.md §2.3.1: "Ranges are requestable" and
/// "the applied ranges" are returned, snapped to what the hardware offers).
pub fn nearest_range(requested_v: f32) -> Range {
    *RANGES
        .iter()
        .min_by(|a, b| {
            (a.range_v - requested_v)
                .abs()
                .total_cmp(&(b.range_v - requested_v).abs())
        })
        .expect("RANGES is non-empty")
}

pub fn default_range() -> Range {
    nearest_range(DEFAULT_RANGE_V)
}

/// `value = raw * scale + offset` for `range` (SPECIFICATION.md §2.3.1),
/// derived from this module's doc comment: `scale = 2*range_v/255`,
/// `offset = -range_v` (`RANGE = 2*range_v`, `RANGE/255` and `-RANGE/2`).
pub fn calibration_for(range: Range) -> (f32, f32) {
    let full_scale = 2.0 * range.range_v; // peak-to-peak, "RANGE" above.
    (full_scale / 255.0, -range.range_v)
}

/// Validates a channel selection against this 2-channel device: non-empty,
/// in range, no duplicates. Order is preserved (it drives calibration and
/// de-interleave order), unlike `fx2lafw`'s bitmask (this device has no
/// hardware channel mask to build).
pub fn validate_channels(channels: &[u8]) -> Result<()> {
    if channels.is_empty()
        || channels.iter().any(|&c| c >= CHANNEL_COUNT)
        || (1..channels.len()).any(|i| channels[i..].contains(&channels[i - 1]))
    {
        return Err(NativeError::InvalidChannelSelection {
            channels: channels.to_vec(),
            channel_count: CHANNEL_COUNT,
        });
    }
    Ok(())
}

/// De-interleaves a raw bulk-transfer buffer of `(CH1, CH2)` byte pairs
/// (SPECIFICATION.md's verified ground truth: the device always sends both
/// channels interleaved, one byte each per timepoint — this module's `0xE4`
/// doc comment) into the wire layout for `channels`:
///
/// - both channels selected: passed through unchanged — already exactly
///   [`awic_proto::frame::Encoding::U8Interleaved`]'s layout.
/// - one channel selected: every other byte, in time order — a single-
///   channel packed stream.
///
/// Trailing odd byte (an incomplete pair) is dropped: it is not a full
/// timepoint for either channel.
pub fn decode_interleaved(raw: &[u8], channels: &[u8]) -> Vec<u8> {
    let pairs = raw.len() / 2;
    match channels {
        [0, 1] => raw[..pairs * 2].to_vec(),
        [ch] => (0..pairs).map(|i| raw[i * 2 + *ch as usize]).collect(),
        // validate_channels() rejects every other shape before this runs.
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_ladder_rate_has_the_traced_register_value() {
        let expected: &[(u64, u8)] = &[
            (48_000_000, 0x30),
            (30_000_000, 30),
            (24_000_000, 0x18),
            (16_000_000, 16),
            (8_000_000, 0x08),
            (4_000_000, 4),
            (1_000_000, 0x01),
            (500_000, 0x32),
            (200_000, 0x14),
            (100_000, 0x0a),
        ];
        for &(rate, reg) in expected {
            assert_eq!(samplerate_reg(rate).unwrap(), reg, "rate {rate}");
        }
    }

    #[test]
    fn sustained_rates_exclude_the_bus_bandwidth_ceiling_rates() {
        let sustained = sustained_rates_hz();
        assert!(!sustained.is_empty());
        assert!(sustained.iter().all(|&r| r <= MAX_SUSTAINED_RATE_HZ));
        // 24/30/48 MHz are valid firmware commands (still in SAMPLE_RATES_HZ)
        // but the driver refuses them (bus-bandwidth ceiling, this module's
        // own doc comment) — a UI defaulting to "the fastest offered rate"
        // must not be handed one of them (caught live: it was).
        assert!(!sustained.contains(&48_000_000));
        assert!(!sustained.contains(&30_000_000));
        assert!(!sustained.contains(&24_000_000));
        assert!(sustained.contains(&MAX_SUSTAINED_RATE_HZ));
        assert_eq!(
            sustained,
            SAMPLE_RATES_HZ
                .iter()
                .copied()
                .filter(|&r| r <= MAX_SUSTAINED_RATE_HZ)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn rejects_a_rate_outside_the_ladder() {
        assert!(samplerate_reg(10_000_000).is_err());
        assert!(samplerate_reg(0).is_err());
    }

    #[test]
    fn range_codes_match_the_traced_values() {
        assert_eq!(nearest_range(5.0).code, 1); // 1 V/div
        assert_eq!(nearest_range(2.5).code, 2); // 500 mV/div, default
        assert_eq!(nearest_range(1.25).code, 5); // 250 mV/div
        assert_eq!(nearest_range(0.5).code, 10); // 100 mV/div
    }

    #[test]
    fn nearest_range_snaps_an_unsupported_request_to_the_closest_offered() {
        // Requesting exactly what the fx2lafw driver's earlier bug assumed
        // (peak-to-peak read as full scale) must still snap to a real range,
        // never silently accept an unreachable value.
        assert_eq!(nearest_range(10.0).range_v, 5.0);
        assert_eq!(nearest_range(0.0).range_v, 0.5);
        assert_eq!(nearest_range(1.3).range_v, 1.25);
    }

    #[test]
    fn default_range_is_500_mv_per_div() {
        let r = default_range();
        assert_eq!(r.code, 2);
        assert_eq!(r.vdiv_v, 0.5);
    }

    #[test]
    fn calibration_reproduces_the_traced_formula_bounds() {
        // 500 mV/div: RANGE = 5 Vpp, so raw 0 -> -2.5 V, raw 255 -> +2.5 V.
        let (scale, offset) = calibration_for(nearest_range(2.5));
        assert!((0.0f32 * scale + offset - (-2.5)).abs() < 1e-4);
        assert!((255.0f32 * scale + offset - 2.5).abs() < 1e-4);
        // The exact centre code (127.5) is not representable; 128 lands a
        // hair above 0 V by construction (see this module's doc comment) --
        // not a bug, and pinned here so a future "fix" doesn't reintroduce
        // one by forcing raw 128 to read exactly 0 V.
        let mid = 128.0f32 * scale + offset;
        assert!(mid > 0.0 && mid < 0.05);
    }

    #[test]
    fn range_v_is_five_times_vdiv_never_peak_to_peak_or_vdiv_itself() {
        // The exact bug SPECIFICATION.md §2.3.1 warns about: range_v must be
        // the +/- full-scale value used for scale/offset, not vdiv (would be
        // 10x too small) and not the peak-to-peak span (would be 2x the
        // correct amplitude, halving every trace).
        for r in RANGES {
            assert_eq!(r.range_v, 5.0 * r.vdiv_v);
        }
    }

    #[test]
    fn validate_channels_accepts_every_legal_subset() {
        validate_channels(&[0]).unwrap();
        validate_channels(&[1]).unwrap();
        validate_channels(&[0, 1]).unwrap();
    }

    #[test]
    fn validate_channels_rejects_empty_out_of_range_or_duplicate() {
        assert!(validate_channels(&[]).is_err());
        assert!(validate_channels(&[2]).is_err());
        assert!(validate_channels(&[0, 0]).is_err());
    }

    #[test]
    fn decode_interleaved_passes_both_channels_through_unchanged() {
        let raw = [10u8, 20, 30, 40, 50, 60];
        assert_eq!(
            decode_interleaved(&raw, &[0, 1]),
            vec![10, 20, 30, 40, 50, 60]
        );
    }

    #[test]
    fn decode_interleaved_extracts_one_channel_in_time_order() {
        let raw = [10u8, 20, 30, 40, 50, 60];
        assert_eq!(decode_interleaved(&raw, &[0]), vec![10, 30, 50]);
        assert_eq!(decode_interleaved(&raw, &[1]), vec![20, 40, 60]);
    }

    #[test]
    fn decode_interleaved_drops_a_trailing_incomplete_pair() {
        let raw = [10u8, 20, 30];
        assert_eq!(decode_interleaved(&raw, &[0, 1]), vec![10, 20]);
        assert_eq!(decode_interleaved(&raw, &[0]), vec![10]);
    }
}
