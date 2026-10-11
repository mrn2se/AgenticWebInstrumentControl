//! Analog capture to digital transitions (GitHub issue #7, "Decoding analog
//! captures"): the front end that lets a protocol decoder, which consumes
//! `(sample_index, packed byte)` transitions, read a line off an oscilloscope
//! capture.
//!
//! Each decoder role bit is a *source*: one hardware channel, or the
//! difference of two (`CANH - CANL`). A source is cut into two logic levels
//! with a Schmitt trigger, the same dead-band idea as `measure`'s edge
//! detector: a sample flips the state only once it clears the *far* side of
//! a band around the threshold, so noise that wobbles around the threshold
//! cannot chatter. The edge is stamped where the signal last crossed the
//! threshold itself, not where it left the band, so a slow edge is not
//! delayed by half its rise time.
//!
//! Unless told otherwise the threshold is automatic and per source: halfway
//! between the line's two dominant levels, found from a histogram of the
//! capture (so a spike, a ringing overshoot or a long idle stretch does not
//! move it). The default dead band is a fifth of that swing, but never less
//! than 1.5 ADC codes: an 8-bit scope quantises a small swing so coarsely
//! that anything narrower would chatter on one code of noise. A source that
//! never leaves one level (a channel not on the bus) has no threshold to
//! find; it reads as one constant level and says so.

use crate::capture_store::{Capture, CaptureData, Transition};
use crate::error::{CoreError, Result};
use crate::series::{hardware_channel_name, hardware_channel_series, hardware_channels, Series};

/// Dead band as a fraction of the swing between the two levels.
const HYSTERESIS_FRACTION: f64 = 0.2;
/// The dead band never gets narrower than this many ADC codes.
const MIN_HYSTERESIS_CODES: f64 = 1.5;
/// A source whose two levels are fewer ADC codes apart than this is flat: it
/// has no second level.
const FLAT_CODES: f64 = 3.0;
/// A flat source is read as a constant high if it sits above this, else low
/// (roughly the TTL input threshold).
const FLAT_HIGH_ABOVE_V: f64 = 1.4;
const BINS: usize = 256;
/// Fraction of samples ignored at each end of the range when locating the
/// levels, so a few spikes cannot stretch the histogram.
const TRIM: f64 = 0.005;
/// …but never more than this many: one short frame on a long idle line can
/// hold well under 0.5 % of the samples at its second level, and trimming a
/// whole plateau would read the line as flat. Spikes are a few samples each.
const MAX_TRIM_SAMPLES: u32 = 64;

/// One decoder role's line: `channel`, or `channel - minus` when `minus` is
/// set. Both are hardware channel numbers (`CH1` is 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnalogSource {
    pub channel: u8,
    pub minus: Option<u8>,
}

/// Threshold overrides, in volts. `None` means automatic. An override
/// applies to every source (for a differential source it is a threshold on
/// the difference).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ThresholdConfig {
    pub threshold_v: Option<f64>,
    /// Total width of the dead band (the Schmitt trigger switches at
    /// `threshold ± hysteresis / 2`).
    pub hysteresis_v: Option<f64>,
    /// For a differential source with no `threshold_v`: the automatic
    /// threshold sits no further from 0 V than this, and its dead band no
    /// wider than `2 × (ceiling − floor)`. Set for CAN (B-99), whose
    /// differential line has a standard threshold — ISO 11898-2: recessive
    /// below 0.5 V, dominant above 0.9 V — where the midpoint of the two
    /// dominant levels is above a weakly driven ACK slot.
    pub differential_ceiling: Option<DifferentialCeiling>,
}

/// See [`ThresholdConfig::differential_ceiling`]; volts, on the difference.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DifferentialCeiling {
    pub threshold_v: f64,
    /// The level the dead band must stay above (recessive's upper bound).
    pub floor_v: f64,
    /// Named in the result's note, e.g. "CAN, ISO 11898-2".
    pub why: &'static str,
}

/// What was used for one source.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SourceThreshold {
    pub source: AnalogSource,
    /// The two dominant levels, volts (about equal for a flat source).
    pub low_v: f64,
    pub high_v: f64,
    /// Infinite for a flat source with no given threshold: it is kept on one
    /// side of the line.
    pub threshold_v: f64,
    pub hysteresis_v: f64,
    /// The source has no second level: it reads as a constant, and (unless
    /// `threshold_v` was given) no threshold was searched for.
    pub flat: bool,
    /// Whether the threshold was supplied rather than found.
    pub explicit: bool,
    /// The automatic threshold was held to the differential ceiling (B-99):
    /// its reason, for the note.
    pub ceiling: Option<&'static str>,
    /// For a differential source: one side that never moves — the
    /// difference is then the other channel alone (a probe off its wire),
    /// as `(hardware channel, level)`.
    pub flat_side: Option<(u8, f64)>,
}

/// Result of [`digitize`]: what a `CaptureData::Digital` would hold, plus
/// the thresholds that produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct Digitized {
    /// The packed byte at sample 0 (bit k = source k), for `Decoder::seed`.
    pub first: u8,
    /// Every change of the packed byte after sample 0.
    pub transitions: Vec<Transition>,
    /// Samples per source: the end sample for the decoder.
    pub samples: u64,
    pub thresholds: Vec<SourceThreshold>,
}

impl Digitized {
    /// One line for a result, e.g. `analog: CH1 threshold 0.420 V,
    /// hysteresis 0.080 V; CH2 flat at 0.370 V (read as constant low)`.
    pub fn describe(&self, capture: &Capture) -> String {
        let parts: Vec<String> = self
            .thresholds
            .iter()
            .map(|t| {
                let name = source_name(capture, t.source);
                let mut part = if t.flat && !t.explicit {
                    let level = if t.low_v > FLAT_HIGH_ABOVE_V {
                        "high"
                    } else {
                        "low"
                    };
                    format!("{name} flat at {:.3} V (read as constant {level})", t.low_v)
                } else {
                    format!(
                        "{name} threshold {:.3} V{}, hysteresis {:.3} V",
                        t.threshold_v,
                        if t.explicit {
                            " (given)".to_string()
                        } else if let Some(why) = t.ceiling {
                            format!(
                                " (standard differential threshold, {why}; the midpoint of the \
                                 two levels, {:.3} V, would be above a weakly driven bit)",
                                (t.low_v + t.high_v) / 2.0
                            )
                        } else {
                            String::new()
                        },
                        t.hysteresis_v
                    )
                };
                if let Some((channel, level)) = t.flat_side {
                    part.push_str(&format!(
                        " — {} is flat at {level:.3} V, so this difference is {} alone: is \
                         its probe on the wire?",
                        hardware_channel_name(capture, channel),
                        hardware_channel_name(
                            capture,
                            if channel == t.source.channel {
                                t.source.minus.unwrap_or(channel)
                            } else {
                                t.source.channel
                            }
                        ),
                    ));
                }
                part
            })
            .collect();
        format!("analog: {}", parts.join("; "))
    }
}

fn source_name(capture: &Capture, source: AnalogSource) -> String {
    let plus = hardware_channel_name(capture, source.channel);
    match source.minus {
        Some(m) => format!("{plus}-{}", hardware_channel_name(capture, m)),
        None => plus,
    }
}

/// Volts per ADC code of hardware channel `channel` (its calibration scale).
fn code_step(capture: &Capture, channel: u8) -> f64 {
    let CaptureData::Analog { channel_scale, .. } = &capture.data else {
        return 0.0;
    };
    let position = hardware_channels(capture)
        .iter()
        .position(|&c| c == channel)
        .unwrap_or(0);
    f64::from(*channel_scale.get(position).unwrap_or(&0.0)).abs()
}

fn volts(capture: &Capture, channel: u8) -> Result<Vec<f64>> {
    match hardware_channel_series(capture, channel)? {
        Series::Volts(v) => Ok(v),
        Series::Levels(_) => Err(not_analog()),
    }
}

fn not_analog() -> CoreError {
    CoreError::Config("analog thresholding needs an analog capture".into())
}

/// The source's samples in volts, and the voltage of one ADC code on it.
fn source_volts(capture: &Capture, source: AnalogSource) -> Result<(Vec<f64>, f64)> {
    let plus = volts(capture, source.channel)?;
    let step = code_step(capture, source.channel);
    let Some(minus) = source.minus else {
        return Ok((plus, step));
    };
    let minus_v = volts(capture, minus)?;
    let diff = plus.iter().zip(&minus_v).map(|(a, b)| a - b).collect();
    Ok((diff, step + code_step(capture, minus)))
}

/// Turns the analog capture into the transitions a digital one holds: bit k
/// of the packed byte is `sources[k]`'s logic level. At most 8 sources.
pub fn digitize(
    capture: &Capture,
    sources: &[AnalogSource],
    config: &ThresholdConfig,
) -> Result<Digitized> {
    if !matches!(capture.data, CaptureData::Analog { .. }) {
        return Err(not_analog());
    }
    if sources.len() > 8 {
        return Err(CoreError::Config(format!(
            "{} sources; a packed byte holds 8",
            sources.len()
        )));
    }
    let mut samples = u64::MAX;
    let mut first = 0u8;
    let mut thresholds = Vec::with_capacity(sources.len());
    // (sample, source bit, new level), from every source.
    let mut changes: Vec<(u64, u8, bool)> = Vec::new();
    for (bit, &source) in sources.iter().enumerate() {
        let (values, step) = source_volts(capture, source)?;
        samples = samples.min(values.len() as u64);
        let mut t = find_thresholds(&values, step, source, config);
        if let Some(minus) = source.minus {
            for side in [source.channel, minus] {
                let (low, high) = dominant_levels(&volts(capture, side)?);
                let step = code_step(capture, side).max(f64::MIN_POSITIVE);
                if high - low < FLAT_CODES * step && !t.flat {
                    t.flat_side = Some((side, (low + high) / 2.0));
                }
            }
        }
        let (initial, edges) = schmitt(&values, &t);
        first |= u8::from(initial) << bit;
        changes.extend(edges.into_iter().map(|(i, level)| (i, bit as u8, level)));
        thresholds.push(t);
    }
    if sources.is_empty() {
        samples = 0;
    }
    changes.sort_unstable();
    let mut transitions: Vec<Transition> = Vec::new();
    let mut state = first;
    for (index, bit, level) in changes {
        if index >= samples {
            continue;
        }
        state = (state & !(1 << bit)) | (u8::from(level) << bit);
        match transitions.last_mut() {
            Some(last) if last.sample_index == index => last.channel_mask = state,
            _ => transitions.push(Transition {
                sample_index: index,
                channel_mask: state,
            }),
        }
    }
    Ok(Digitized {
        first,
        transitions,
        samples,
        thresholds,
    })
}

/// The levels, threshold and dead band for one source.
fn find_thresholds(
    values: &[f64],
    step: f64,
    source: AnalogSource,
    config: &ThresholdConfig,
) -> SourceThreshold {
    let (low, high) = dominant_levels(values);
    let step = step.max(f64::MIN_POSITIVE);
    let flat = high - low < FLAT_CODES * step;
    let default_hysteresis = if flat {
        MIN_HYSTERESIS_CODES * step
    } else {
        (HYSTERESIS_FRACTION * (high - low)).max(MIN_HYSTERESIS_CODES * step)
    };
    let mut threshold_v = match config.threshold_v {
        Some(v) => v,
        // A flat source has no midpoint; the threshold only has to keep it
        // on one side of the line.
        None if flat && low > FLAT_HIGH_ABOVE_V => f64::NEG_INFINITY,
        None if flat => f64::INFINITY,
        None => (low + high) / 2.0,
    };
    let mut hysteresis_v = config.hysteresis_v.unwrap_or(default_hysteresis).max(0.0);
    // B-99: on a differential line the recessive level is the one at 0 V and
    // the dominant one away from it — above for CANH-CANL, below for
    // CANL-CANH. The threshold moves toward 0 V, never past the standard one;
    // a small swing (a 10:1 probe the scope cannot be told about) keeps its
    // midpoint, which is then already closer to 0 V.
    let mut ceiling = None;
    if let (Some(c), Some(_), None, false) = (
        config.differential_ceiling,
        source.minus,
        config.threshold_v,
        flat,
    ) {
        let dominant_below = low.abs() > high.abs();
        let (limit, floor) = if dominant_below {
            (-c.threshold_v, -c.floor_v)
        } else {
            (c.threshold_v, c.floor_v)
        };
        if threshold_v.abs() > limit.abs() {
            threshold_v = limit;
            ceiling = Some(c.why);
            if config.hysteresis_v.is_none() {
                hysteresis_v = hysteresis_v.min(2.0 * (limit - floor).abs());
            }
        }
    }
    SourceThreshold {
        source,
        low_v: low,
        high_v: high,
        threshold_v,
        hysteresis_v,
        flat,
        explicit: config.threshold_v.is_some(),
        ceiling,
        flat_side: None,
    }
}

/// The two dominant levels `(low, high)`: the most populated bin of the
/// lower and of the upper half of the (outlier-trimmed) range. Both equal
/// the constant for a constant signal.
fn dominant_levels(values: &[f64]) -> (f64, f64) {
    let (min, max) = values
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &v| {
            (lo.min(v), hi.max(v))
        });
    if values.is_empty() {
        return (0.0, 0.0);
    }
    if max <= min {
        return (min, max);
    }
    let histogram = |lo: f64, hi: f64| {
        let width = (hi - lo) / BINS as f64;
        let mut bins = [0u32; BINS];
        for &v in values {
            let bin = ((v - lo) / width).floor().clamp(0.0, (BINS - 1) as f64) as usize;
            bins[bin] += 1;
        }
        (bins, width)
    };
    // Pass 1 finds the trimmed range, pass 2 resolves the plateaus in it.
    let (coarse, width) = histogram(min, max);
    let cut = ((values.len() as f64 * TRIM) as u32).min(MAX_TRIM_SAMPLES);
    let mut seen = 0;
    let first_bin = coarse
        .iter()
        .position(|&c| {
            seen += c;
            seen > cut
        })
        .unwrap_or(0);
    seen = 0;
    let last_bin = coarse
        .iter()
        .rposition(|&c| {
            seen += c;
            seen > cut
        })
        .unwrap_or(BINS - 1);
    let lo = min + first_bin as f64 * width;
    let hi = (min + (last_bin + 1) as f64 * width).max(lo + width);
    let (bins, width) = histogram(lo, hi);
    let mode = |range: std::ops::Range<usize>| {
        let best = range
            .clone()
            .fold(range.start, |b, i| if bins[i] > bins[b] { i } else { b });
        lo + (best as f64 + 0.5) * width
    };
    (mode(0..BINS / 2), mode(BINS / 2..BINS))
}

/// Runs the Schmitt trigger over `values`: the level of sample 0 and every
/// later change as `(sample, new level)`.
fn schmitt(values: &[f64], t: &SourceThreshold) -> (bool, Vec<(u64, bool)>) {
    let Some(&first) = values.first() else {
        return (false, Vec::new());
    };
    let half = t.hysteresis_v / 2.0;
    let (upper, lower) = (t.threshold_v + half, t.threshold_v - half);
    let mut level = first >= t.threshold_v;
    let initial = level;
    let mut edges = Vec::new();
    // First sample since the signal was last on its settled side of the
    // threshold itself: where the edge really happened.
    let mut crossed: Option<usize> = None;
    for (i, &v) in values.iter().enumerate() {
        let on_settled_side = (v >= t.threshold_v) == level;
        if on_settled_side {
            crossed = None;
        } else if crossed.is_none() {
            crossed = Some(i);
        }
        let flips = if level { v <= lower } else { v >= upper };
        if flips && !on_settled_side {
            edges.push((crossed.unwrap_or(i) as u64, !level));
            level = !level;
            crossed = None;
        }
    }
    (initial, edges)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture_store::CaptureMeta;
    use crate::{CaptureId, DeviceId};
    use awic_proto::frame::Encoding;

    /// 8-bit codes for a 5 V range, as the oscilloscope reports them.
    const SCALE: f32 = 10.0 / 255.0;
    const OFFSET: f32 = -5.0;

    fn code(v: f64) -> u8 {
        ((v - f64::from(OFFSET)) / f64::from(SCALE))
            .round()
            .clamp(0.0, 255.0) as u8
    }

    fn meta(channels: Vec<u8>) -> CaptureMeta {
        CaptureMeta {
            device: DeviceId::new(3, vec![1], 0, 0),
            rate_hz: 1_000_000,
            channel_count: channels.len() as u8,
            channels,
            encoding: Encoding::U8Interleaved,
            t0_ns: 0,
            trigger_position: None,
        }
    }

    /// An 8-bit analog capture of `lines` (one `Vec` of volts per channel).
    fn capture8(lines: &[Vec<f64>]) -> Capture {
        let n = lines.iter().map(Vec::len).min().unwrap();
        let raw = (0..n)
            .flat_map(|i| lines.iter().map(move |l| code(l[i])))
            .collect();
        Capture {
            id: CaptureId::new(),
            meta: meta((0..lines.len() as u8).collect()),
            data: CaptureData::Analog {
                raw,
                sample_bits: 8,
                channel_scale: vec![SCALE; lines.len()],
                channel_offset: vec![OFFSET; lines.len()],
            },
        }
    }

    /// The same as 16-bit samples at 1 mV per code.
    fn capture16(lines: &[Vec<f64>]) -> Capture {
        let n = lines.iter().map(Vec::len).min().unwrap();
        let raw = (0..n)
            .flat_map(|i| {
                lines
                    .iter()
                    .flat_map(move |l| ((l[i] * 1000.0).round() as i16).to_le_bytes())
            })
            .collect();
        Capture {
            id: CaptureId::new(),
            meta: CaptureMeta {
                encoding: Encoding::I16,
                ..meta((0..lines.len() as u8).collect())
            },
            data: CaptureData::Analog {
                raw,
                sample_bits: 16,
                channel_scale: vec![0.001; lines.len()],
                channel_offset: vec![0.0; lines.len()],
            },
        }
    }

    /// Cheap deterministic noise in `[-1, 1]`.
    fn noise(seed: &mut u32) -> f64 {
        *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        f64::from(*seed >> 8) / f64::from(1u32 << 23) - 1.0
    }

    /// `cells` bit cells of `width` samples, `high`/`low` volts, plus noise.
    fn square(cells: &[bool], width: usize, low: f64, high: f64, amp: f64) -> Vec<f64> {
        let mut seed = 7;
        cells
            .iter()
            .flat_map(|&b| std::iter::repeat_n(if b { high } else { low }, width))
            .map(|v| v + amp * noise(&mut seed))
            .collect()
    }

    const PATTERN: [bool; 10] = [
        true, false, true, true, false, false, true, false, true, true,
    ];

    fn src(channel: u8) -> AnalogSource {
        AnalogSource {
            channel,
            minus: None,
        }
    }

    fn edges(d: &Digitized) -> Vec<(u64, u8)> {
        d.transitions
            .iter()
            .map(|t| (t.sample_index, t.channel_mask))
            .collect()
    }

    const CAN: ThresholdConfig = ThresholdConfig {
        threshold_v: None,
        hysteresis_v: None,
        differential_ceiling: Some(DifferentialCeiling {
            threshold_v: 0.7,
            floor_v: 0.5,
            why: "CAN",
        }),
    };

    /// CANH and CANL of a frame whose bits swing 2.4 V differential, and an
    /// ACK slot (cells 7-8) driven by a weak node to only 1.1 V (B-99).
    fn weak_ack(scale: f64) -> (Vec<f64>, Vec<f64>) {
        let cells = [
            false, true, false, true, true, false, false, true, true, false, false,
        ];
        let mut h = Vec::new();
        let mut l = Vec::new();
        for (i, &dominant) in cells.iter().enumerate() {
            let swing = match (dominant, i) {
                (false, _) => 0.0,
                (true, 7 | 8) => 1.1,
                (true, _) => 2.4,
            };
            for _ in 0..20 {
                h.push(2.5 + scale * swing / 2.0);
                l.push(2.5 - scale * swing / 2.0);
            }
        }
        (h, l)
    }

    fn diff(plus: u8, minus: u8) -> AnalogSource {
        AnalogSource {
            channel: plus,
            minus: Some(minus),
        }
    }

    /// B-99: the midpoint of the two dominant levels (1.2 V) is above the
    /// weak ACK slot; the CAN ceiling holds the threshold at 0.7 V, inside
    /// ISO 11898-2's window, and the ACK is seen. Mirrored for CANL - CANH.
    #[test]
    fn a_can_differential_threshold_stays_below_a_weak_ack() {
        let (h, l) = weak_ack(1.0);
        let c = capture8(&[h, l]);
        let plain = digitize(&c, &[diff(0, 1)], &ThresholdConfig::default()).unwrap();
        assert!(
            plain.thresholds[0].threshold_v > 1.1,
            "{:?}",
            plain.thresholds[0]
        );
        assert_eq!(
            edges(&plain).len(),
            4,
            "the weak ACK is lost: {:?}",
            edges(&plain)
        );
        let can = digitize(&c, &[diff(0, 1)], &CAN).unwrap();
        let t = can.thresholds[0];
        assert_eq!((t.threshold_v, t.ceiling), (0.7, Some("CAN")));
        assert!(t.hysteresis_v <= 0.4 + 1e-9, "{t:?}");
        assert_eq!(edges(&can).len(), 6, "{:?}", edges(&can));
        assert!(can.describe(&c).contains("standard differential threshold"));
        let mirrored = digitize(&c, &[diff(1, 0)], &CAN).unwrap();
        assert_eq!(mirrored.thresholds[0].threshold_v, -0.7);
        assert_eq!(edges(&mirrored).len(), 6);
    }

    /// Through 10:1 probes the scope cannot be told about, the same bus
    /// swings a tenth as much: its midpoint is already below the ceiling and
    /// stays; an explicit threshold always wins.
    #[test]
    fn the_can_ceiling_leaves_a_small_swing_and_a_given_threshold_alone() {
        let (h, l) = weak_ack(0.1);
        let c = capture16(&[h, l]);
        let t = digitize(&c, &[diff(0, 1)], &CAN).unwrap().thresholds[0];
        assert!(t.threshold_v < 0.13 && t.ceiling.is_none(), "{t:?}");
        let (h, l) = weak_ack(1.0);
        let c = capture8(&[h, l]);
        let given = ThresholdConfig {
            threshold_v: Some(1.5),
            ..CAN
        };
        let t = digitize(&c, &[diff(0, 1)], &given).unwrap().thresholds[0];
        assert_eq!((t.threshold_v, t.ceiling), (1.5, None));
    }

    /// #32: with CANL's probe off its wire, CANH - CANL is CANH alone; the
    /// note says so instead of only giving a threshold.
    #[test]
    fn a_flat_side_of_a_difference_is_named() {
        let (h, _) = weak_ack(1.0);
        let flat = vec![0.3; h.len()];
        let c = capture8(&[h, flat]);
        let d = digitize(&c, &[diff(0, 1)], &CAN).unwrap();
        assert_eq!(d.thresholds[0].flat_side.map(|f| f.0), Some(1));
        let note = d.describe(&c);
        assert!(
            note.contains("CH2 is flat") && note.contains("CH1 alone"),
            "{note}"
        );
    }

    #[test]
    fn one_short_burst_on_a_long_idle_line_is_not_flat() {
        // 1000 dominant samples in 400 000: 0.25 %, under the 0.5 % trim.
        let mut line = vec![3.3; 400_000];
        line[200_000..201_000].fill(0.2);
        let c = capture8(&[line]);
        let d = digitize(&c, &[src(0)], &ThresholdConfig::default()).unwrap();
        assert!(!d.thresholds[0].flat, "{:?}", d.thresholds[0]);
        assert_eq!(edges(&d), vec![(200_000, 0), (201_000, 1)]);
    }

    #[test]
    fn clean_square_gives_edges_at_the_crossings() {
        let c = capture8(&[square(&PATTERN, 20, 0.2, 3.1, 0.0)]);
        let d = digitize(&c, &[src(0)], &ThresholdConfig::default()).unwrap();
        assert_eq!(d.first, 1);
        assert_eq!(d.samples, 200);
        // Transitions at the starts of cells 1, 2, 4, 6, 7, 8.
        let at: Vec<u64> = d.transitions.iter().map(|t| t.sample_index).collect();
        assert_eq!(at, vec![20, 40, 80, 120, 140, 160]);
        let levels: Vec<u8> = d.transitions.iter().map(|t| t.channel_mask).collect();
        assert_eq!(levels, vec![0, 1, 0, 1, 0, 1]);
        let t = d.thresholds[0];
        assert!(!t.flat && !t.explicit);
        assert!((t.threshold_v - 1.65).abs() < 0.05, "{t:?}");
        assert!(d.describe(&c).starts_with("analog: CH1 threshold 1.6"));
    }

    #[test]
    fn noise_does_not_chatter() {
        // +-0.45 V of noise on a 2.9 V swing: a bare comparator would flip
        // on every sample near the edges; the band must not.
        let c = capture8(&[square(&PATTERN, 40, 0.2, 3.1, 0.45)]);
        let d = digitize(&c, &[src(0)], &ThresholdConfig::default()).unwrap();
        let at: Vec<i64> = d
            .transitions
            .iter()
            .map(|t| t.sample_index as i64)
            .collect();
        let expected = [40, 80, 160, 240, 280, 320];
        assert_eq!(at.len(), expected.len(), "{at:?}");
        for (got, want) in at.iter().zip(expected) {
            assert!((got - want).abs() <= 2, "{at:?}");
        }
    }

    #[test]
    fn a_small_swing_on_8_bits_keeps_the_minimum_band() {
        // 4 codes of swing with one code of noise: 20 % of the swing is less
        // than a code, the 1.5-code floor is what holds it together.
        let mut seed = 3;
        let mut raw = Vec::new();
        for &b in &PATTERN {
            for _ in 0..32 {
                let base: i32 = if b { 141 } else { 137 };
                raw.push((base + (noise(&mut seed) * 1.2).round() as i32) as u8);
            }
        }
        let c = Capture {
            id: CaptureId::new(),
            meta: meta(vec![0]),
            data: CaptureData::Analog {
                raw,
                sample_bits: 8,
                channel_scale: vec![SCALE],
                channel_offset: vec![OFFSET],
            },
        };
        let d = digitize(&c, &[src(0)], &ThresholdConfig::default()).unwrap();
        assert_eq!(d.transitions.len(), 6, "{:?}", edges(&d));
        let t = d.thresholds[0];
        assert!(t.hysteresis_v >= 1.5 * f64::from(SCALE) - 1e-9, "{t:?}");
    }

    #[test]
    fn inverted_signal_gives_inverted_bits() {
        let cells: Vec<bool> = PATTERN.iter().map(|b| !b).collect();
        let a = digitize(
            &capture8(&[square(&PATTERN, 20, 0.2, 3.1, 0.0)]),
            &[src(0)],
            &ThresholdConfig::default(),
        )
        .unwrap();
        let b = digitize(
            &capture8(&[square(&cells, 20, 0.2, 3.1, 0.0)]),
            &[src(0)],
            &ThresholdConfig::default(),
        )
        .unwrap();
        assert_eq!(a.first ^ 1, b.first);
        assert_eq!(a.transitions.len(), b.transitions.len());
        for (x, y) in a.transitions.iter().zip(&b.transitions) {
            assert_eq!(x.sample_index, y.sample_index);
            assert_eq!(x.channel_mask ^ 1, y.channel_mask);
        }
    }

    #[test]
    fn a_flat_line_is_a_constant_with_a_note() {
        for (volts, bit) in [(0.37, 0u8), (3.3, 1u8)] {
            let c = capture8(&[vec![volts; 500]]);
            let d = digitize(&c, &[src(0)], &ThresholdConfig::default()).unwrap();
            assert!(d.transitions.is_empty());
            assert_eq!(d.first, bit);
            assert!(d.thresholds[0].flat);
            assert!(d.describe(&c).contains("flat at"), "{}", d.describe(&c));
        }
        // With one code of noise too.
        let mut seed = 1;
        let noisy: Vec<f64> = (0..500).map(|_| 3.3 + 0.03 * noise(&mut seed)).collect();
        let d = digitize(&capture8(&[noisy]), &[src(0)], &Default::default()).unwrap();
        assert!(d.transitions.is_empty());
        assert_eq!(
            digitize(&capture8(&[vec![1.0]]), &[src(0)], &Default::default())
                .unwrap()
                .transitions
                .len(),
            0
        );
    }

    #[test]
    fn a_spike_does_not_move_the_threshold() {
        let mut v = square(&PATTERN, 100, 0.2, 3.1, 0.0);
        v[500] = 4.9;
        v[501] = 4.9;
        let c = capture8(&[v]);
        let d = digitize(&c, &[src(0)], &Default::default()).unwrap();
        assert!((d.thresholds[0].threshold_v - 1.65).abs() < 0.1);
    }

    #[test]
    fn differential_pair_reads_the_difference() {
        // CANH 2.5 -> 3.5 V, CANL 2.5 -> 1.5 V on a dominant bit; common
        // mode of 2.5 V that a single-ended threshold could not separate.
        let dominant = [false, true, true, false, true, false];
        let h = square(&dominant, 30, 2.5, 3.5, 0.0);
        let l = square(&dominant, 30, 2.5, 1.5, 0.0);
        let c = capture8(&[h, l]);
        let d = digitize(
            &c,
            &[AnalogSource {
                channel: 0,
                minus: Some(1),
            }],
            &ThresholdConfig::default(),
        )
        .unwrap();
        assert_eq!(d.first, 0);
        let at: Vec<u64> = d.transitions.iter().map(|t| t.sample_index).collect();
        assert_eq!(at, vec![30, 90, 120, 150]);
        assert!(
            d.describe(&c).contains("CH1-CH2 threshold 1.0"),
            "{}",
            d.describe(&c)
        );
    }

    #[test]
    fn two_sources_pack_into_one_byte_and_coincident_edges_merge() {
        let a = square(&[false, true, true, false], 25, 0.2, 3.1, 0.0);
        let b = square(&[false, true, false, false], 25, 0.2, 3.1, 0.0);
        let d = digitize(&capture8(&[a, b]), &[src(0), src(1)], &Default::default()).unwrap();
        assert_eq!(edges(&d), vec![(25, 0b11), (50, 0b01), (75, 0b00)]);
        // Roles may name channels in any order.
        let a = square(&[false, true, true, false], 25, 0.2, 3.1, 0.0);
        let b = square(&[false, true, false, false], 25, 0.2, 3.1, 0.0);
        let d = digitize(&capture8(&[a, b]), &[src(1), src(0)], &Default::default()).unwrap();
        assert_eq!(edges(&d), vec![(25, 0b11), (50, 0b10), (75, 0b00)]);
    }

    #[test]
    fn explicit_threshold_and_hysteresis_are_used_as_given() {
        let c = capture8(&[square(&PATTERN, 20, 0.2, 3.1, 0.0)]);
        // A threshold above the high level: never goes high.
        let d = digitize(
            &c,
            &[src(0)],
            &ThresholdConfig {
                threshold_v: Some(4.0),
                hysteresis_v: Some(0.1),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(d.transitions.is_empty());
        let t = d.thresholds[0];
        assert!(t.explicit);
        assert_eq!((t.threshold_v, t.hysteresis_v), (4.0, 0.1));
        assert!(d.describe(&c).contains("(given)"));
        // A band wider than the swing around the middle blocks every edge,
        // while the default finds them.
        let d = digitize(
            &c,
            &[src(0)],
            &ThresholdConfig {
                threshold_v: None,
                hysteresis_v: Some(4.0),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(d.transitions.is_empty());
        // A low explicit threshold moves the edge with it.
        let ramp: Vec<f64> = (0..100).map(|i| f64::from(i) * 0.04).collect();
        let c = capture8(&[ramp]);
        let low = digitize(
            &c,
            &[src(0)],
            &ThresholdConfig {
                threshold_v: Some(1.0),
                hysteresis_v: Some(0.1),
                ..Default::default()
            },
        )
        .unwrap();
        let high = digitize(
            &c,
            &[src(0)],
            &ThresholdConfig {
                threshold_v: Some(3.0),
                hysteresis_v: Some(0.1),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(low.transitions[0].sample_index < high.transitions[0].sample_index);
    }

    #[test]
    fn sixteen_bit_captures_threshold_alike() {
        let c = capture16(&[square(&PATTERN, 20, 0.2, 3.1, 0.05)]);
        let d = digitize(&c, &[src(0)], &Default::default()).unwrap();
        let at: Vec<u64> = d.transitions.iter().map(|t| t.sample_index).collect();
        assert_eq!(at, vec![20, 40, 80, 120, 140, 160]);
        assert!((d.thresholds[0].threshold_v - 1.65).abs() < 0.05);
    }

    #[test]
    fn hardware_channel_numbers_are_translated() {
        // A capture of channels [1, 2]: hardware channel 2 is position 1.
        let mut c = capture8(&[
            vec![1.0; 100],
            square(&[false, true, false, true], 25, 0.2, 3.1, 0.0),
        ]);
        c.meta.channels = vec![1, 2];
        let d = digitize(&c, &[src(2)], &Default::default()).unwrap();
        assert_eq!(d.transitions.len(), 3);
        assert!(digitize(&c, &[src(0)], &Default::default()).is_err());
    }

    #[test]
    fn refuses_digital_captures_and_too_many_sources() {
        let digital = Capture {
            id: CaptureId::new(),
            meta: meta(vec![0]),
            data: CaptureData::Digital {
                packed: vec![0, 1],
                transitions: vec![],
            },
        };
        assert!(digitize(&digital, &[src(0)], &Default::default()).is_err());
        let c = capture8(&[vec![0.0; 10]]);
        assert!(digitize(&c, &[src(0); 9], &Default::default()).is_err());
    }
}
