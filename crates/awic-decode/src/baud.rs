//! UART bit-rate detection from one line's pulse widths (live test F-05).
//!
//! Every pulse on a UART line — a run of equal bits between two edges — lasts
//! a whole number of bit times. The shortest pulses are single bits, so a low
//! percentile of the pulse widths is a first estimate of one bit time; the
//! estimate is then refined over every pulse that sits near an integer
//! multiple of it (a least-squares fit of `width = n × bit`), and accepted only
//! if most pulses do. That last check is what tells a UART line from a clock,
//! PWM, or I²C line, whose pulse widths are not integer multiples of anything
//! in particular.
//!
//! Limits, by construction: a stream whose every pulse is two or more bits
//! long (only `0x00`/`0xFF`-like bytes) reads as half the real rate, and a
//! line needs enough traffic — [`MIN_PULSES`] pulses — to say anything.

use crate::Transition;

/// Pulses narrower than this many samples are glitches (crosstalk, ringing),
/// not bits: no bit rate is detected closer to the sample rate than that.
pub const GLITCH_SAMPLES: u64 = 2;
/// Fewer pulses than this and the estimate is refused rather than guessed.
pub const MIN_PULSES: usize = 8;
/// Share of pulses that must land within [`MULTIPLE_TOLERANCE`] of an integer
/// number of bits for the line to count as UART-like.
pub const MIN_FIT: f64 = 0.85;
/// How far (in bit times) a pulse may be from an integer multiple.
const MULTIPLE_TOLERANCE: f64 = 0.2;
/// The longest pulse considered part of a frame: start + 9 data + parity is 11
/// bits; idle gaps are longer and carry no timing information.
const MAX_BITS_PER_PULSE: f64 = 11.0;
/// Times the bit time is refitted over the pulses it explains (B-81).
const REFINE_ROUNDS: usize = 4;
/// A detected rate within this fraction of a standard rate is reported as it.
const SNAP_TOLERANCE: f64 = 0.03;

/// Common UART rates, for snapping a measured rate to what was configured.
pub const STANDARD_BAUDS: &[u32] = &[
    300, 600, 1200, 2400, 4800, 9600, 14_400, 19_200, 28_800, 31_250, 38_400, 57_600, 74_880,
    76_800, 115_200, 128_000, 230_400, 250_000, 256_000, 460_800, 500_000, 921_600, 1_000_000,
    1_500_000, 2_000_000, 3_000_000,
];

/// Standard CAN bit rates, for snapping a measured rate to what was configured.
pub const STANDARD_CAN_BITRATES: &[u32] = &[
    10_000, 20_000, 50_000, 83_333, 100_000, 125_000, 250_000, 500_000, 800_000, 1_000_000,
];

/// A detected bit rate and how well the line fit it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BaudEstimate {
    /// The reported rate: the nearest standard rate if within 3 %, else the
    /// measured rate rounded.
    pub baud: u32,
    /// The rate the pulse widths themselves say, before snapping.
    pub measured: f64,
    /// Share of the considered pulses that fit an integer number of bits.
    pub fit: f64,
    /// Pulses the estimate was made from (glitches and idle gaps excluded).
    pub pulses: usize,
}

/// Widths, in samples, of every complete pulse on `bit` — the time between
/// consecutive edges of that bit. The partial runs before the first and after
/// the last edge are not pulses (their true length is unknown).
pub fn pulse_widths(transitions: &[Transition], bit: u8) -> Vec<u64> {
    let mut widths = Vec::new();
    let mut prev: Option<bool> = None;
    let mut last_edge: Option<u64> = None;
    for t in transitions {
        let level = (t.value >> bit) & 1 == 1;
        match prev {
            None => prev = Some(level),
            Some(p) if p != level => {
                if let Some(e) = last_edge {
                    widths.push(t.sample_index - e);
                }
                last_edge = Some(t.sample_index);
                prev = Some(level);
            }
            _ => {}
        }
    }
    widths
}

/// Folds every glitch (a pulse of ≤ [`GLITCH_SAMPLES`]) back into its two
/// neighbours: a spike in the middle of a bit splits one real pulse into two
/// meaningless ones. `widths` must be in time order (alternating levels).
pub fn merge_glitches(widths: &[u64]) -> Vec<u64> {
    let mut out: Vec<u64> = Vec::with_capacity(widths.len());
    let mut i = 0;
    while i < widths.len() {
        let w = widths[i];
        match (out.last_mut(), widths.get(i + 1)) {
            (Some(prev), Some(&next)) if w <= GLITCH_SAMPLES => {
                *prev += w + next;
                i += 2;
            }
            _ => {
                out.push(w);
                i += 1;
            }
        }
    }
    out
}

/// Detects a UART bit rate from pulse widths (samples, in time order) at
/// `rate_hz`, or `None` if there are too few pulses or they don't look like
/// UART bits.
pub fn estimate_from_widths(widths: &[u64], rate_hz: u64) -> Option<BaudEstimate> {
    estimate_snapping(widths, rate_hz, STANDARD_BAUDS)
}

/// Detects a CAN bit rate: the shortest stable pulse is one bit, and every
/// pulse on a stuffed bus is one to six of them, so the fit that tells UART
/// from a clock tells CAN from one too. Snaps to [`STANDARD_CAN_BITRATES`].
pub fn estimate_can_from_widths(widths: &[u64], rate_hz: u64) -> Option<BaudEstimate> {
    estimate_snapping(widths, rate_hz, STANDARD_CAN_BITRATES)
}

/// [`pulse_widths`] + [`estimate_can_from_widths`].
pub fn estimate_can_bitrate(
    transitions: &[Transition],
    bit: u8,
    rate_hz: u64,
) -> Option<BaudEstimate> {
    estimate_can_from_widths(&pulse_widths(transitions, bit), rate_hz)
}

fn estimate_snapping(widths: &[u64], rate_hz: u64, standards: &[u32]) -> Option<BaudEstimate> {
    let mut clean: Vec<u64> = merge_glitches(widths)
        .into_iter()
        .filter(|&w| w > GLITCH_SAMPLES)
        .collect();
    if clean.len() < MIN_PULSES {
        return None;
    }
    clean.sort_unstable();
    // A low percentile rather than the minimum: one short glitch that
    // survived the filter must not halve the estimate.
    let (mut bit_samples, mut fit, mut considered) =
        fit_bit_time(&clean, clean[clean.len() / 20] as f64)?;
    // More than one in twenty pulses can be runts — a bit-banged line
    // catching up after losing its CPU left six half-bit pulses among 97 on
    // the ESP32 bench — and half-bit runts fit a grid of half bits, which
    // every real pulse also fits as an even number of units ("19200 Bd" for
    // 9600). Real UART at its own rate has plenty of odd runs (1, 3, 5 bits);
    // when almost none are odd, the unit is half a bit.
    if odd_share(&clean, bit_samples) < 1.0 - MIN_FIT {
        if let Some(doubled) =
            fit_bit_time(&clean, 2.0 * bit_samples).filter(|&(_, f, _)| f >= MIN_FIT)
        {
            (bit_samples, fit, considered) = doubled;
        }
    }
    if fit < MIN_FIT {
        return one_level_estimate(widths, rate_hz, standards);
    }
    let measured = rate_hz as f64 / bit_samples;
    Some(BaudEstimate {
        baud: snap_to_standard(measured, standards),
        measured,
        fit,
        pulses: considered,
    })
}

/// The bit rate from one level's runs alone, when both together do not fit
/// (B-108). Only the active level's runs are whole numbers of bits: a stop
/// bit runs on into whatever idle follows it, which a bit-banged transmitter
/// leaves between bytes (a 9600 Bd line from Lua: stop bits of 1.36 bits),
/// and a jittery line's two-edge widths stray further than one level's do.
/// A single level fits a clock trivially — all its pulses are one length —
/// so a level counts only if its runs look like data: one-bit and longer
/// runs both common, in at least three different lengths.
fn one_level_estimate(widths: &[u64], rate_hz: u64, standards: &[u32]) -> Option<BaudEstimate> {
    let merged = merge_glitches(widths);
    (0..2)
        .filter_map(|parity| {
            let mut level: Vec<u64> = merged
                .iter()
                .skip(parity)
                .step_by(2)
                .copied()
                .filter(|&w| w > GLITCH_SAMPLES)
                .collect();
            if level.len() < MIN_PULSES {
                return None;
            }
            level.sort_unstable();
            let (bit_samples, fit, considered) =
                fit_bit_time(&level, level[level.len() / 20] as f64)?;
            (fit >= MIN_FIT && looks_like_data(&level, bit_samples)).then_some((
                bit_samples,
                fit,
                considered,
            ))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(bit_samples, fit, pulses)| {
            let measured = rate_hz as f64 / bit_samples;
            BaudEstimate {
                baud: snap_to_standard(measured, standards),
                measured,
                fit,
                pulses,
            }
        })
}

/// Whether the runs fitting `bit_samples` look like data rather than a
/// clock: single bits and longer runs each at least a tenth of them, in
/// three or more different lengths.
fn looks_like_data(level: &[u64], bit_samples: f64) -> bool {
    let mut lengths = std::collections::BTreeSet::new();
    let (mut single, mut longer, mut fitting) = (0usize, 0usize, 0usize);
    for &w in level {
        let bits = w as f64 / bit_samples;
        let n = bits.round().max(1.0);
        if bits <= MAX_BITS_PER_PULSE + 0.5 && (bits - n).abs() <= MULTIPLE_TOLERANCE {
            fitting += 1;
            lengths.insert(n as u32);
            if n == 1.0 {
                single += 1;
            } else {
                longer += 1;
            }
        }
    }
    let share = |k: usize| k as f64 / fitting.max(1) as f64;
    lengths.len() >= 3 && share(single) >= 0.1 && share(longer) >= 0.1
}

/// The bit time that best explains `clean` (sorted, glitches removed),
/// starting from the pulse width `low`: `(bit_samples, fit, considered)`,
/// or `None` when too few pulses are frame-sized.
fn fit_bit_time(clean: &[u64], low: f64) -> Option<(f64, f64, usize)> {
    // The mean of every pulse near `low` as the first bit time. At 16
    // samples per bit one-bit pulses measure 15..17: the 5th percentile
    // alone says 15, and a 5-bit pulse of 80 then reads as 5.33 bits and
    // fails the whole-number test. Within a quarter of it only, so two pulse
    // widths that are no multiple of a common bit (a PWM's 37 and 53) stay
    // apart — and not far below it either, so runts do not drag a guess
    // that was doubled from them back down.
    let near: Vec<f64> = clean
        .iter()
        .map(|&w| w as f64)
        .filter(|&w| (0.8..1.25).contains(&(w / low)))
        .collect();
    let mut bit_samples = near.iter().sum::<f64>() / near.len().max(1) as f64;

    // The short pulses alone can be a few percent off — a bit-banged line's
    // one-bit pulses run short by the write's own cost — and that error grows
    // with the multiple: 2400 Bd runs of 8 bits read as 8.27 "bits" of a
    // 9600 Bd burst's 1-bit pulses and failed the test (B-81). Refit with the
    // least-squares bit time until it settles.
    let mut fit = 0.0;
    let mut considered = 0;
    for _ in 0..REFINE_ROUNDS {
        let (mut sum_w, mut sum_n, mut fitting) = (0.0, 0.0, 0usize);
        considered = 0;
        for &w in clean {
            let bits = w as f64 / bit_samples;
            if bits > MAX_BITS_PER_PULSE + 0.5 {
                continue; // an idle gap
            }
            considered += 1;
            let n = bits.round().max(1.0);
            if (bits - n).abs() <= MULTIPLE_TOLERANCE {
                fitting += 1;
                sum_w += w as f64;
                sum_n += n;
            }
        }
        if considered < MIN_PULSES || sum_n == 0.0 {
            return None;
        }
        fit = fitting as f64 / considered as f64;
        let refined = sum_w / sum_n;
        let settled = (refined - bit_samples).abs() < 1e-9 * bit_samples;
        bit_samples = refined;
        if settled {
            break;
        }
    }
    Some((bit_samples, fit, considered))
}

/// Share of the pulses fitting `bit_samples` whose length is an odd number
/// of it.
fn odd_share(clean: &[u64], bit_samples: f64) -> f64 {
    let (mut odd, mut fitting) = (0usize, 0usize);
    for &w in clean {
        let bits = w as f64 / bit_samples;
        let n = bits.round().max(1.0);
        if bits <= MAX_BITS_PER_PULSE + 0.5 && (bits - n).abs() <= MULTIPLE_TOLERANCE {
            fitting += 1;
            odd += usize::from(n as u64 % 2 == 1);
        }
    }
    odd as f64 / fitting.max(1) as f64
}

/// [`pulse_widths`] + [`estimate_from_widths`].
pub fn estimate_baud(transitions: &[Transition], bit: u8, rate_hz: u64) -> Option<BaudEstimate> {
    estimate_from_widths(&pulse_widths(transitions, bit), rate_hz)
}

fn snap_to_standard(measured: f64, standards: &[u32]) -> u32 {
    standards
        .iter()
        .copied()
        .find(|&b| ((measured - b as f64) / b as f64).abs() <= SNAP_TOLERANCE)
        .unwrap_or_else(|| measured.round() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 8N1 frames of `bytes` on bit 0 at `bit` samples per bit, idle high,
    /// with `gap` idle bits between frames.
    fn uart_line(bytes: &[u8], bit: f64, gap: usize) -> Vec<Transition> {
        let mut levels = Vec::new();
        levels.extend(std::iter::repeat_n(true, 10));
        for &b in bytes {
            levels.push(false);
            for i in 0..8 {
                levels.push((b >> i) & 1 == 1);
            }
            levels.push(true);
            levels.extend(std::iter::repeat_n(true, gap));
        }
        let mut out = Vec::new();
        let mut prev = None;
        for (i, &l) in levels.iter().enumerate() {
            if prev != Some(l) {
                out.push(Transition {
                    sample_index: (i as f64 * bit).round() as u64,
                    value: u8::from(l),
                });
                prev = Some(l);
            }
        }
        out
    }

    /// B-81, the ESP32 bench at 16 MS/s: a bit-banged 2400 Bd burst, then a
    /// 9600 Bd one 20 ms later, in one window. The 9600 Bd one-bit pulses run
    /// ~3 % short, so the 2400 Bd runs of 8 units read 8.27 against the first
    /// estimate and the fit fell to 0.83. Refitting settles on 9600.
    #[test]
    fn a_window_with_two_rates_still_fits_their_common_bit() {
        let widths = [
            26168, 6714, 13306, 6729, 6615, 6703, 6690, 6616, 6633, 6714, 13300, 13344, 6688, 6631,
            20017, 13317, 6703, 13319, 6714, 6631, 20026, 13319, 6614, 13416, 6631, 6637, 6714,
            26616, 6703, 13330, 6702, 6672, 39945, 6714, 13320, 6681, 6604, 6715, 33323, 6727,
            6616, 6679, 6714, 19935, 6709, 6591, 6714, 6630, 6688, 6714, 6616, 6702, 13330, 6694,
            13246, 6689, 6714, 6627, 6688, 13344, 26664, 6689, 6679, 6616, 40037, 6688, 13304,
            6689, 6630, 6714, 19998, 13319, 13287, 6715, 13318, 6631, 13375, 13344, 13297, 6729,
            6590, 13344, 13399, 13304, 13331, 6703, 6615, 6729, 6591, 13335, 26718, 6630, 13319,
            6714, 6630, 6689, 26615, 334277, 6587, 1715, 3333, 1618, 1715, 1633, 1617, 1710, 1715,
            1618, 8435, 732, 343, 540, 4951, 3333, 1715, 3333, 1632, 1618, 5023, 3333, 1715, 3446,
            1519, 1716, 1617, 6738, 1617, 3348, 1617, 1716, 9991, 1617, 3323, 1716, 1715, 1618,
            8298, 1716, 1617, 1716, 1684, 4951, 1715, 1618, 1729, 1618, 1716, 1617, 1716, 1617,
            3307, 1716, 3347, 1618, 1715, 1618, 1715, 3333, 6655, 1617, 1716, 1618, 10014, 1716,
            3333, 1681, 1618, 1715, 4966, 3333, 3333, 1618, 3431, 1593, 3347, 3333, 3333, 1711,
            1618, 3333, 3347, 3308, 3333, 1715, 1618, 1716, 1617, 3347, 6642, 1715, 3333, 1618,
            1730, 1617, 6667,
        ];
        let e = estimate_from_widths(&widths, 16_000_000).expect("detected");
        assert_eq!(e.baud, 9600, "{e:?}");
    }

    /// Found re-checking B-81 on the ESP32 bench: a 9600 Bd burst whose
    /// bit-banging loop lost the CPU for ~1 ms and caught up with six
    /// sub-bit pulses (25-50 µs). The 5th percentile is a half-bit runt, and
    /// every real pulse is an even number of those: "19200 Bd, fit 0.94".
    #[test]
    fn half_bit_runts_do_not_double_the_rate() {
        let widths = [
            6667, 1593, 3336, 1725, 1593, 1704, 1725, 1586, 1725, 1592, 3318, 3442, 1574, 1724,
            4978, 3316, 1725, 3313, 1705, 1592, 5040, 3317, 1593, 3558, 1461, 1836, 1593, 6633,
            1593, 3427, 1593, 1725, 9952, 1724, 3368, 1592, 1724, 1593, 8333, 1724, 1593, 1724,
            1718, 4909, 1665, 1719, 1705, 1593, 1724, 1593, 1719, 1593, 3449, 1593, 3294, 1724,
            1725, 1592, 1722, 3256, 6745, 1593, 1724, 1593, 10062, 1593, 3389, 1725, 1592, 1725,
            5022, 3317, 3318, 1592, 3449, 1593, 3292, 15710, 816, 407, 407, 815, 816, 1079, 3318,
            1719, 1593, 1724, 1593, 3425, 6635, 1722, 3257, 1725, 1573, 1722, 6635,
        ];
        let e = estimate_from_widths(&widths, 16_000_000).expect("detected");
        assert_eq!(e.baud, 9600, "{e:?}");
    }

    #[test]
    fn can_bit_time_is_the_mean_of_one_bit_pulses_not_the_shortest() {
        // 500 kbit/s at 8 MS/s from the oscilloscope on a real bus: one-bit pulses
        // quantise to 15..17 samples, longer ones to whole multiples of 16.
        let mut widths = Vec::new();
        for _ in 0..20 {
            widths.extend([16, 15, 17, 16, 32, 48, 80, 64, 16, 15, 31, 33, 81, 47]);
        }
        let e = estimate_can_from_widths(&widths, 8_000_000).unwrap();
        assert_eq!(e.baud, 500_000, "{e:?}");
    }

    #[test]
    fn detects_115200_at_12_mhz() {
        // 12 MHz / 115200 = 104.17 samples per bit: not an integer, as on
        // the live bench.
        let t = uart_line(b"help\r\nHello, world!", 12e6 / 115_200.0, 3);
        let est = estimate_baud(&t, 0, 12_000_000).expect("a UART line");
        assert_eq!(est.baud, 115_200);
        assert!((est.measured - 115_200.0).abs() < 200.0, "{est:?}");
        assert!(est.fit > 0.95, "{est:?}");
    }

    #[test]
    fn detects_9600_and_an_odd_rate() {
        let t = uart_line(b"AWIC AWIC AWIC", 1e6 / 9600.0, 0);
        assert_eq!(estimate_baud(&t, 0, 1_000_000).unwrap().baud, 9600);
        // 100 000 baud is no standard rate: reported as measured.
        let t = uart_line(b"The quick brown fox", 24e6 / 100_000.0, 1);
        assert_eq!(estimate_baud(&t, 0, 24_000_000).unwrap().baud, 100_000);
    }

    #[test]
    fn ignores_glitches() {
        let mut t = uart_line(b"glitchy line!", 12e6 / 115_200.0, 2);
        // Two-sample spikes after a few edges, as crosstalk produces.
        let spikes: Vec<Transition> = t
            .iter()
            .step_by(5)
            .flat_map(|e| {
                let v = e.value;
                [
                    Transition {
                        sample_index: e.sample_index + 20,
                        value: v ^ 1,
                    },
                    Transition {
                        sample_index: e.sample_index + 22,
                        value: v,
                    },
                ]
            })
            .collect();
        t.extend(spikes);
        t.sort_by_key(|x| x.sample_index);
        assert_eq!(estimate_baud(&t, 0, 12_000_000).unwrap().baud, 115_200);
    }

    /// Levels of a CAN-like burst: pulses of 1..=5 bits, as stuffing allows,
    /// with a long recessive gap after every other burst.
    fn can_like_line(bit: f64, bursts: usize) -> Vec<Transition> {
        let mut out = Vec::new();
        let (mut at, mut level) = (0.0f64, true);
        let mut x: u32 = 12345;
        for burst in 0..bursts {
            for _ in 0..40 {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
                let bits = 1 + (x >> 16) % 5;
                out.push(Transition {
                    sample_index: at.round() as u64,
                    value: u8::from(level),
                });
                at += f64::from(bits) * bit;
                level = !level;
            }
            if burst % 2 == 0 {
                at += 20.0 * bit;
            }
        }
        out
    }

    #[test]
    fn detects_standard_can_bitrates() {
        // 16 MS/s / 500k = 32 samples/bit; 125k = 128; 83.333k = 192.
        for (rate, bit, expect) in [
            (16_000_000u64, 32.0, 500_000u32),
            (16_000_000, 128.0, 125_000),
            (16_000_000, 192.0, 83_333),
            (16_000_000, 16.0, 1_000_000),
            (4_000_000, 16.0, 250_000),
        ] {
            let t = can_like_line(bit, 6);
            let est = estimate_can_bitrate(&t, 0, rate).expect("a CAN line");
            assert_eq!(est.baud, expect, "{est:?}");
        }
        // A 1 % fast transmitter still snaps.
        let t = can_like_line(32.0 * 0.99, 6);
        assert_eq!(
            estimate_can_bitrate(&t, 0, 16_000_000).unwrap().baud,
            500_000
        );
    }

    #[test]
    fn a_clock_is_not_uart() {
        // A PWM whose high and low times (37 and 53 samples) are no integer
        // multiple of a common bit time.
        let mut t = Vec::new();
        let mut s = 0;
        for i in 0..100 {
            t.push(Transition {
                sample_index: s,
                value: (i % 2) as u8,
            });
            s += if i % 2 == 0 { 37 } else { 53 };
        }
        assert_eq!(estimate_baud(&t, 0, 1_000_000), None);
    }

    /// B-108: a bit-banged 9600 Bd line (the ESP32 bench from Lua) leaves
    /// 0.36 bit of idle after every stop bit, and its one-bit pulses measure
    /// 0.83..1.15 bits. The high runs ending in a stop bit then fit no
    /// whole number of bits and the line failed as a whole; its low runs —
    /// start bit and zeros, always whole bits — carry the rate.
    #[test]
    fn a_stretched_stop_bit_still_reads_as_uart() {
        let bit = 104.0;
        let mut levels: Vec<(bool, f64)> = Vec::new(); // (level, length in bits)
        let text = b"t=1234 ms temp=21.5 C\r\n";
        for (i, &b) in text.iter().cycle().take(120).enumerate() {
            levels.push((false, 1.0));
            for k in 0..8 {
                levels.push(((b >> k) & 1 == 1, 1.0));
            }
            levels.push((true, 1.36 + if i % 23 == 22 { 400.0 } else { 0.0 }));
        }
        let mut t = Vec::new();
        let (mut pos, mut prev) = (0.0, None);
        for (j, &(level, bits)) in levels.iter().enumerate() {
            if prev != Some(level) {
                // Up to ±0.08 bit on each edge, not accumulating: widths
                // stray by up to ±0.16 bit, as on the bench.
                let jitter = [0.0, 0.08, -0.06, 0.03, -0.08, 0.05][j % 6] * bit;
                t.push(Transition {
                    sample_index: (pos + jitter) as u64,
                    value: u8::from(level),
                });
                prev = Some(level);
            }
            pos += bits * bit;
        }
        let est = estimate_baud(&t, 0, 1_000_000).expect("a UART line");
        assert_eq!(est.baud, 9600, "{est:?}");
    }

    #[test]
    fn too_little_traffic_is_refused() {
        let t = uart_line(b"A", 100.0, 0);
        assert_eq!(estimate_baud(&t, 0, 1_000_000), None);
    }
}
