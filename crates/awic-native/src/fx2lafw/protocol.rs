//! The `fx2lafw` application-firmware protocol (SPECIFICATION.md §2.5),
//! for the fx2lafw logic analyzer at `0925:3881` (8 digital channels, 8-bit
//! samples).
//!
//! Protocol reference: read (not copied — see this crate's top-level
//! licensing note) from `libsigrok`'s `src/hardware/fx2lafw/protocol.h` and
//! `protocol.c`, which give the command numbers, the `cmd_start_acquisition`
//! wire struct, and the clock-divisor arithmetic below. Every number here is
//! independently checkable against the running firmware (see
//! `command_get_firmware_version` used in `driver.rs`, and this module's own
//! tests), which is the verification SPECIFICATION.md §2.5 calls for rather
//! than trusting a single documentary source.

use crate::error::{NativeError, Result};

/// `bRequest` values. All three are vendor, device-recipient requests
/// (`bmRequestType` 0xC0 for the two reads, 0x40 for the write), `wValue`
/// and `wIndex` both `0x0000`.
pub const CMD_GET_FW_VERSION: u8 = 0xB0;
pub const CMD_START: u8 = 0xB1;
pub const CMD_GET_REVID_VERSION: u8 = 0xB2;

/// The firmware's own major/minor version, returned by `CMD_GET_FW_VERSION`
/// as two raw bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirmwareVersion {
    pub major: u8,
    pub minor: u8,
}

impl FirmwareVersion {
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 2 {
            return Err(NativeError::Transfer(nusb::transfer::TransferError::Fault));
        }
        Ok(Self {
            major: data[0],
            minor: data[1],
        })
    }
}

/// The major firmware version this driver's protocol assumptions target.
/// Logged as a warning (not a hard failure — the wire format has been
/// stable across the fx2lafw project's history) when it doesn't match.
pub const EXPECTED_FW_VERSION_MAJOR: u8 = 1;

/// `USB_INTERFACE`: the (sole) interface `fx2lafw` firmware exposes.
pub const INTERFACE_NUMBER: u8 = 0;

/// This device's channel count (SPECIFICATION.md §2.5's verified ground
/// truth: "fx2lafw logic analyzer: 8 channels: D0-D7"). One sample is one byte, one
/// bit per channel, which is also exactly `awic_core::CaptureData::Digital`
/// packed representation — no reshaping needed between wire and store.
pub const CHANNEL_COUNT: u8 = 8;

/// Bit positions within `cmd_start_acquisition.flags`. `CLK_CTL2` and
/// `WIDE_16BIT` are documented here for completeness (and so a future
/// mixed-signal/16-bit variant of this driver has them ready) but this
/// driver is 8-bit, digital-only, and never sets either.
#[allow(dead_code)]
mod flags {
    /// Set when at least one *analog* channel is enabled (mixed-signal FX2
    /// variants only). This driver is digital-only, so it never sets this.
    pub const CLK_CTL2: u8 = 1 << 4;
    /// Sample width: clear = 8-bit samples (our only mode; the fx2lafw logic analyzer
    /// has no `DEV_CAPS_16BIT`), set = 16-bit.
    pub const WIDE_16BIT: u8 = 1 << 5;
    /// Base clock select: clear = 30 MHz, set = 48 MHz.
    pub const CLK_48MHZ: u8 = 1 << 6;
}

/// The maximum GPIF sample delay the firmware accepts (`6 * 256` — the
/// delay counter's effective range). A samplerate whose divisor exceeds
/// this cannot be reached from either base clock.
const MAX_SAMPLE_DELAY: u32 = 6 * 256;

const CLOCK_48MHZ_HZ: u64 = 48_000_000;
const CLOCK_30MHZ_HZ: u64 = 30_000_000;

/// The sample-rate ladder this firmware actually exposes. Every entry here
/// must round-trip through [`start_command`] to a delay `<= MAX_SAMPLE_DELAY`
/// against one of the two base clocks — enforced by this module's own test —
/// so this list can't silently drift out of sync with the arithmetic below.
pub const SAMPLE_RATES_HZ: &[u64] = &[
    20_000, 25_000, 50_000, 100_000, 200_000, 250_000, 500_000, 1_000_000, 2_000_000, 3_000_000,
    4_000_000, 6_000_000, 8_000_000, 12_000_000, 16_000_000, 24_000_000, 48_000_000,
];

/// The highest rate 8 channels actually sustain on this device.
///
/// 48 MHz is a valid firmware command and stays in the ladder above for
/// completeness, but 8 bits x 48 MHz = 48 MB/s exceeds what USB 2.0 bulk
/// sustains: the FX2's 4 KB FIFO overruns before the host's first poll and the
/// device sends nothing at all (0 bytes, measured).
///
/// 24 MHz was the ceiling until B-63. It *can* stream - 24 M samples in 1.01 s
/// with zero dropped transfers, measured on an otherwise quiet bus - but on a
/// bus with anything else on it, it mostly does not: 12 of 23 acquisitions
/// stalled in one session, against 0 of 21 at every rate from 20 kHz to
/// 16 MHz, and each stall costs a firmware re-upload and a re-enumeration.
/// 16 MHz is the fastest rate that did not fail once, and this constant's own
/// rule - a rate that always fails is not offered - applies to one that fails
/// half the time on a bus shared with the instrument it is being used next to.
pub const MAX_SUSTAINED_RATE_HZ: u64 = 16_000_000;

/// The rate ladder actually worth offering in the UI (SPECIFICATION.md §5.2):
/// every entry of [`SAMPLE_RATES_HZ`] the device can sustain,
/// i.e. up to [`MAX_SUSTAINED_RATE_HZ`]. Computed rather than duplicated so
/// it can't drift from either constant.
/// The next rate down the ladder from `rate_hz`, for telling a caller what
/// to try when the device will not start at the one they asked for.
pub fn next_lower_rate_hz(rate_hz: u64) -> Option<u64> {
    SAMPLE_RATES_HZ
        .iter()
        .copied()
        .filter(|&r| r < rate_hz)
        .max()
}

pub fn sustained_rates_hz() -> Vec<u64> {
    SAMPLE_RATES_HZ
        .iter()
        .copied()
        .filter(|&r| r <= MAX_SUSTAINED_RATE_HZ)
        .collect()
}

/// The 3-byte payload of `CMD_START`: `{ flags, sample_delay_h, sample_delay_l }`
/// (big-endian 16-bit delay split across the last two bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartCommand {
    pub flags: u8,
    pub sample_delay: u16,
}

impl StartCommand {
    pub fn to_bytes(self) -> [u8; 3] {
        let [hi, lo] = self.sample_delay.to_be_bytes();
        [self.flags, hi, lo]
    }
}

/// Maps a requested sample rate to the `CMD_START` payload that drives the
/// FX2's GPIF state machine at that rate.
///
/// The firmware derives its sample clock by dividing a 48 MHz or 30 MHz base
/// clock by `sample_delay + 1`; a rate is reachable only if it divides one
/// of those two evenly *and* the resulting delay fits in the counter's
/// range. 48 MHz is preferred (tried first) since it is the finer-grained
/// base and is what real fx2lafw hosts use for the common power-of-two-ish
/// rates; 30 MHz covers the handful of rates (e.g. 20 kHz, 25 kHz) that
/// don't divide 48 MHz within range but do divide 30 MHz.
pub fn start_command(rate_hz: u64) -> Result<StartCommand> {
    if rate_hz == 0 {
        return Err(NativeError::UnsupportedSampleRate { rate_hz });
    }

    for (clock_hz, clock_flag) in [(CLOCK_48MHZ_HZ, flags::CLK_48MHZ), (CLOCK_30MHZ_HZ, 0u8)] {
        if clock_hz % rate_hz != 0 {
            continue;
        }
        let divisor = clock_hz / rate_hz;
        let Some(delay) = divisor.checked_sub(1) else {
            continue;
        };
        if delay <= u64::from(MAX_SAMPLE_DELAY) {
            return Ok(StartCommand {
                // 8-bit, all-digital: WIDE_16BIT and CLK_CTL2 stay clear.
                flags: clock_flag,
                sample_delay: delay as u16,
            });
        }
    }

    Err(NativeError::UnsupportedSampleRate { rate_hz })
}

/// Applies a channel selection to a raw byte captured off the wire: each
/// wire byte already has one bit per channel (D0 = bit 0 ... D7 = bit 7,
/// SPECIFICATION.md §2.5), so "decoding" a packed sample is exactly masking
/// off channels the caller didn't select — the on-wire and stored
/// (`CaptureData::Digital`) representations are otherwise identical.
///
/// `mask` is a bitmask of enabled channels, built once per acquisition via
/// [`channel_mask`].
#[inline]
pub fn decode_sample(raw_byte: u8, mask: u8) -> u8 {
    raw_byte & mask
}

/// Decodes a whole buffer in place order, returning a new `Vec` — used when
/// handing a completed USB transfer's bytes to the `CaptureStore` /
/// `SampleSink` (both want one packed byte per sample, same as the wire).
pub fn decode_samples(raw: &[u8], mask: u8) -> Vec<u8> {
    raw.iter().map(|&b| decode_sample(b, mask)).collect()
}

/// Builds the enabled-channel bitmask `CMD_START`'s data doesn't carry (the
/// firmware always samples and sends all 8 lines; channel selection is a
/// purely host-side post-filter, applied by [`decode_samples`]) from an
/// `AcqConfig.channels` list, validating every entry is in range.
pub fn channel_mask(channels: &[u8]) -> Result<u8> {
    if channels.is_empty() || channels.iter().any(|&c| c >= CHANNEL_COUNT) {
        return Err(NativeError::InvalidChannelSelection {
            channels: channels.to_vec(),
            channel_count: CHANNEL_COUNT,
        });
    }
    Ok(channels.iter().fold(0u8, |mask, &c| mask | (1 << c)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_ladder_rate_has_a_reachable_start_command() {
        for &rate in SAMPLE_RATES_HZ {
            let cmd = start_command(rate)
                .unwrap_or_else(|e| panic!("rate {rate} Hz must be reachable: {e}"));
            // Recompute the effective rate from the command and check it's
            // exact, not just "close enough" — a logic analyzer's whole
            // value proposition is knowing precisely when a sample was
            // taken.
            let clock = if cmd.flags & flags::CLK_48MHZ != 0 {
                CLOCK_48MHZ_HZ
            } else {
                CLOCK_30MHZ_HZ
            };
            let effective_rate = clock / (u64::from(cmd.sample_delay) + 1);
            assert_eq!(effective_rate, rate, "rate {rate} Hz round-trip mismatch");
        }
    }

    #[test]
    fn prefers_the_48mhz_clock_when_both_are_exact() {
        // 1 MHz divides both 48 MHz (delay 47) and 30 MHz (delay 29); the
        // implementation must pick 48 MHz first per the doc comment.
        let cmd = start_command(1_000_000).unwrap();
        assert_ne!(cmd.flags & flags::CLK_48MHZ, 0);
        assert_eq!(cmd.sample_delay, 47);
    }

    #[test]
    fn falls_back_to_30mhz_when_48mhz_does_not_divide_evenly() {
        // 25 kHz: 48 MHz / 25 kHz = 1920 exactly, delay 1919 > MAX_SAMPLE_DELAY
        // (1536), so it must fall back to 30 MHz (delay 1199).
        let cmd = start_command(25_000).unwrap();
        assert_eq!(cmd.flags & flags::CLK_48MHZ, 0);
        assert_eq!(cmd.sample_delay, 1199);
    }

    #[test]
    fn rejects_a_rate_neither_clock_divides_evenly() {
        assert!(start_command(700_000).is_err());
    }

    #[test]
    fn rejects_zero_rate() {
        assert!(start_command(0).is_err());
    }

    #[test]
    fn start_command_serializes_big_endian_delay_after_flags() {
        let cmd = StartCommand {
            flags: 0b0100_0000,
            sample_delay: 0x0102,
        };
        assert_eq!(cmd.to_bytes(), [0b0100_0000, 0x01, 0x02]);
    }

    #[test]
    fn channel_mask_covers_selected_channels_only() {
        assert_eq!(channel_mask(&[0, 2, 7]).unwrap(), 0b1000_0101);
        assert_eq!(channel_mask(&(0..8).collect::<Vec<_>>()).unwrap(), 0xFF);
    }

    #[test]
    fn channel_mask_rejects_out_of_range_or_empty_selection() {
        assert!(channel_mask(&[]).is_err());
        assert!(channel_mask(&[8]).is_err());
    }

    #[test]
    fn decode_masks_off_unselected_channels_bit_for_bit() {
        let mask = channel_mask(&[0, 1]).unwrap();
        assert_eq!(decode_sample(0b1111_1111, mask), 0b0000_0011);
        assert_eq!(decode_sample(0b0000_0000, mask), 0);
        assert_eq!(
            decode_samples(&[0xFF, 0x00, 0b1010_1010], mask),
            vec![0b11, 0x00, 0b10]
        );
    }

    #[test]
    fn firmware_version_parses_major_minor_bytes() {
        let v = FirmwareVersion::parse(&[1, 7]).unwrap();
        assert_eq!(v, FirmwareVersion { major: 1, minor: 7 });
    }

    #[test]
    fn firmware_version_rejects_short_response() {
        assert!(FirmwareVersion::parse(&[1]).is_err());
    }

    #[test]
    fn next_lower_rate_is_the_neighbouring_rung() {
        assert_eq!(next_lower_rate_hz(24_000_000), Some(16_000_000));
        assert_eq!(next_lower_rate_hz(16_000_000), Some(12_000_000));
        // Below the bottom rung there is nothing to suggest.
        assert_eq!(next_lower_rate_hz(*SAMPLE_RATES_HZ.first().unwrap()), None);
        // A rate that is not on the ladder still gets the rung below it.
        assert_eq!(next_lower_rate_hz(20_000_000), Some(16_000_000));
    }

    /// B-63: 24 MHz was offered as sustained and stalled 12 times in 23
    /// acquisitions on a bus shared with the scope, against 0 of 21 at every
    /// rate up to 16 MHz. The ladder stops at 16 MHz now, and the two rates
    /// above it stay in the full ladder (a valid firmware command) without
    /// being offered.
    #[test]
    fn the_sustained_ladder_stops_below_the_rates_that_stall() {
        assert_eq!(MAX_SUSTAINED_RATE_HZ, 16_000_000);
        let sustained = sustained_rates_hz();
        assert!(sustained.contains(&16_000_000));
        assert!(!sustained.contains(&24_000_000), "{sustained:?}");
        assert!(!sustained.contains(&48_000_000), "{sustained:?}");
        // Still on the full ladder, so `start_command` builds one for it and
        // `next_lower_rate_hz` can point at a rung below.
        assert!(SAMPLE_RATES_HZ.contains(&24_000_000));
        assert_eq!(next_lower_rate_hz(24_000_000), Some(16_000_000));
    }

    #[test]
    fn sustained_rates_are_a_prefix_of_the_full_ladder_up_to_the_max() {
        let sustained = sustained_rates_hz();
        assert!(!sustained.is_empty());
        assert!(sustained.iter().all(|&r| r <= MAX_SUSTAINED_RATE_HZ));
        assert!(sustained.contains(&1_000_000));
        assert_eq!(*sustained.last().unwrap(), MAX_SUSTAINED_RATE_HZ);
        assert_eq!(
            sustained,
            SAMPLE_RATES_HZ
                .iter()
                .copied()
                .filter(|&r| r <= MAX_SUSTAINED_RATE_HZ)
                .collect::<Vec<_>>()
        );
    }
}
