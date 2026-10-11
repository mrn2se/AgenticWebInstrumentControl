//! One channel of a capture as a plain series, in physical units
//! (SPECIFICATION.md §2.7): volts for analog, levels for digital.
//!
//! This exists so that everything which has to *interpret* samples —
//! `measure` and the renderer — reads them through one function. The analog
//! conversion is `raw * channel_scale[c] + channel_offset[c]` with the
//! capture's own calibration, never derived from bit depth (§2.3.1), and a
//! second copy of that arithmetic somewhere else is a second chance for a
//! plausible, wrong number.

use crate::capture_store::{Capture, CaptureData};
use crate::error::CoreError;

/// One channel's samples in physical units.
#[derive(Debug, Clone, PartialEq)]
pub enum Series {
    /// Analog samples, volts.
    Volts(Vec<f64>),
    /// Digital samples, one level per sample.
    Levels(Vec<bool>),
}

impl Series {
    pub fn len(&self) -> usize {
        match self {
            Series::Volts(v) => v.len(),
            Series::Levels(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The series as numbers, so a caller that treats both kinds alike (a
    /// plot, a histogram) needs no match. Digital levels become 0.0/1.0.
    pub fn to_f64(&self) -> Vec<f64> {
        match self {
            Series::Volts(v) => v.clone(),
            Series::Levels(v) => v.iter().map(|&b| f64::from(u8::from(b))).collect(),
        }
    }

    /// The unit these numbers are in, for axis labels.
    pub fn unit(&self) -> &'static str {
        match self {
            Series::Volts(_) => "V",
            Series::Levels(_) => "",
        }
    }
}

/// `channel` is a position in the interleave, `0..meta.channel_count` — for
/// analog the same addressing `measure_capture` documents, and for digital
/// the bit number inside the packed byte (§3.2).
pub fn channel_series(capture: &Capture, channel: u8) -> Result<Series, CoreError> {
    match &capture.data {
        CaptureData::Analog {
            raw,
            sample_bits,
            channel_scale,
            channel_offset,
        } => {
            let count = capture.meta.channel_count;
            if count == 0 || channel >= count {
                return Err(channel_not_found(
                    capture,
                    channel,
                    format!("{count} analog channel(s), addressed 0..{count}"),
                ));
            }
            let n = count as usize;
            let scale = f64::from(*channel_scale.get(channel as usize).unwrap_or(&1.0));
            let offset = f64::from(*channel_offset.get(channel as usize).unwrap_or(&0.0));
            let volts = if *sample_bits <= 8 {
                raw.iter()
                    .skip(channel as usize)
                    .step_by(n)
                    .map(|&s| f64::from(s) * scale + offset)
                    .collect()
            } else {
                let (chunks, _remainder) = raw.as_chunks::<2>();
                chunks
                    .iter()
                    .map(|c| i16::from_le_bytes(*c))
                    .skip(channel as usize)
                    .step_by(n)
                    .map(|s| f64::from(s) * scale + offset)
                    .collect()
            };
            Ok(Series::Volts(volts))
        }
        CaptureData::Digital { packed, .. } => {
            if channel > 7 {
                return Err(channel_not_found(
                    capture,
                    channel,
                    "digital channels are bits 0-7 of the packed byte",
                ));
            }
            let mask = 1u8 << channel;
            Ok(Series::Levels(
                packed.iter().map(|&b| b & mask != 0).collect(),
            ))
        }
    }
}

/// Smallest share of samples on a converter end code that counts as clipping.
///
/// A signal that merely touches full scale crosses the end code and leaves it
/// again; one that is *past* the range sits on it. One stray sample is the
/// first, a thousandth of the capture is the second.
const CLIPPED_SHARE: f64 = 0.001;

/// How much of one analog channel sits on the ends of its converter's range.
///
/// The oscilloscope at its 2.5 V range reading a 2.7-4.1 V bus returns code 255 for
/// every sample: `vpp` 0, `vmin` = `vmax` = 2.50 V, and timing refused as
/// "under the 4 codes timing needs" - all true of the capture and none of it
/// about the signal (B-54). The codes themselves are the only place this is
/// visible, so it is detected here, where they are read.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Clipping {
    /// Samples on the lowest code this sample width can hold.
    pub at_bottom: usize,
    /// Samples on the highest code.
    pub at_top: usize,
    pub total: usize,
    /// What those end codes are worth through this channel's own calibration.
    pub bottom_v: f64,
    pub top_v: f64,
}

impl Clipping {
    /// Share of the capture sitting on either end code, 0.0..=1.0.
    pub fn share(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        (self.at_bottom + self.at_top) as f64 / self.total as f64
    }

    /// The sentence this must not be read without: which rail, how much of
    /// the capture, and what to do about it.
    pub fn describe(&self) -> String {
        let pct = |n: usize| n as f64 / self.total.max(1) as f64 * 100.0;
        let rail = match (self.at_top, self.at_bottom) {
            (top, 0) if top > 0 => format!(
                "{}on the converter's top code ({:+.2} V)",
                if self.share() >= 1.0 {
                    "every sample ".to_string()
                } else {
                    format!("{:.1} % of samples ", pct(top))
                },
                self.top_v
            ),
            (0, bottom) if bottom > 0 => format!(
                "{}on the converter's bottom code ({:+.2} V)",
                if self.share() >= 1.0 {
                    "every sample ".to_string()
                } else {
                    format!("{:.1} % of samples ", pct(bottom))
                },
                self.bottom_v
            ),
            (top, bottom) => format!(
                "{:.1} % of samples on the converter's top code ({:+.2} V) and {:.1} % on its \
                 bottom code ({:+.2} V)",
                pct(top),
                self.top_v,
                pct(bottom),
                self.bottom_v
            ),
        };
        format!(
            "clipped: {rail} - the signal goes past this range, so its levels are the range's \
             and not the signal's. Capture at a larger range for a true reading"
        )
    }
}

/// Clipping on analog channel `channel` - a position in the interleave, like
/// [`channel_series`]. `None` for a digital capture, a channel out of range,
/// or a channel with less than [`CLIPPED_SHARE`] of its samples on an end
/// code, which is a signal inside the range.
pub fn channel_clipping(capture: &Capture, channel: u8) -> Option<Clipping> {
    let CaptureData::Analog {
        raw,
        sample_bits,
        channel_scale,
        channel_offset,
    } = &capture.data
    else {
        return None;
    };
    let count = capture.meta.channel_count;
    if count == 0 || channel >= count {
        return None;
    }
    let n = count as usize;
    let scale = f64::from(*channel_scale.get(channel as usize).unwrap_or(&1.0));
    let offset = f64::from(*channel_offset.get(channel as usize).unwrap_or(&0.0));
    // Only the two widths the wire format carries (see `view_frame`): unsigned
    // 8-bit codes, or little-endian i16.
    let (bottom_code, top_code, codes): (f64, f64, Vec<f64>) = if *sample_bits <= 8 {
        (
            0.0,
            f64::from(u8::MAX),
            raw.iter()
                .skip(channel as usize)
                .step_by(n)
                .map(|&s| f64::from(s))
                .collect(),
        )
    } else {
        let (chunks, _remainder) = raw.as_chunks::<2>();
        (
            f64::from(i16::MIN),
            f64::from(i16::MAX),
            chunks
                .iter()
                .map(|c| i16::from_le_bytes(*c))
                .skip(channel as usize)
                .step_by(n)
                .map(f64::from)
                .collect(),
        )
    };
    let total = codes.len();
    if total == 0 {
        return None;
    }
    let at_bottom = codes.iter().filter(|&&c| c <= bottom_code).count();
    let at_top = codes.iter().filter(|&&c| c >= top_code).count();
    let clipping = Clipping {
        at_bottom,
        at_top,
        total,
        bottom_v: bottom_code * scale + offset,
        top_v: top_code * scale + offset,
    };
    (clipping.share() >= CLIPPED_SHARE).then_some(clipping)
}

/// [`channel_clipping`] addressed by hardware channel (`CH1` is 0), like
/// [`hardware_channel_series`].
pub fn hardware_channel_clipping(capture: &Capture, channel: u8) -> Option<Clipping> {
    let position = hardware_channels(capture)
        .iter()
        .position(|&c| c == channel)?;
    channel_clipping(capture, position as u8)
}

/// The hardware channels a capture holds, as every tool addresses them:
/// `D4` is 4, `CH1` is 0. Falls back to `0..channel_count` for a capture
/// stored before `meta.channels` was filled in.
pub fn hardware_channels(capture: &Capture) -> Vec<u8> {
    if capture.meta.channels.is_empty() {
        (0..capture.meta.channel_count).collect()
    } else {
        capture.meta.channels.clone()
    }
}

/// `D4`/`CH1`: hardware channel `channel`'s name, as the UI and every tool
/// print it — digital `D<bit>`, analog `CH<n>` counting from 1.
pub fn hardware_channel_name(capture: &Capture, channel: u8) -> String {
    match &capture.data {
        CaptureData::Digital { .. } => format!("D{channel}"),
        CaptureData::Analog { .. } => format!("CH{}", u16::from(channel) + 1),
    }
}

/// Hardware channel `channel` of `capture` (B-33): the same addressing as
/// `measure`, `decode`, `read_capture` and `survey_channels`. Analog
/// channels are translated to their position in the interleave; digital
/// channel `n` is bit `n` of the packed byte. A channel the capture did not
/// acquire is refused with the valid ones listed by name.
pub fn hardware_channel_series(capture: &Capture, channel: u8) -> Result<Series, CoreError> {
    let acquired = hardware_channels(capture);
    let Some(position) = acquired.iter().position(|&c| c == channel) else {
        let names: Vec<String> = acquired
            .iter()
            .map(|&c| format!("{} ({c})", hardware_channel_name(capture, c)))
            .collect();
        return Err(channel_not_found(
            capture,
            channel,
            format!("this capture holds {}", names.join(", ")),
        ));
    };
    match &capture.data {
        CaptureData::Analog { .. } => channel_series(capture, position as u8),
        CaptureData::Digital { .. } => channel_series(capture, channel),
    }
}

fn channel_not_found(capture: &Capture, channel: u8, valid: impl Into<String>) -> CoreError {
    CoreError::ChannelNotFound {
        capture: capture.id.to_string(),
        channel,
        valid: valid.into(),
    }
}

#[cfg(test)]
mod tests {

    /// B-54: the oscilloscope at 2.5 V reading a 2.7-4.1 V bus returns code 255 for
    /// every sample. The codes are the only place that is visible.
    #[test]
    fn a_capture_pinned_to_the_top_code_is_clipped() {
        let capture = clip_capture(vec![255; 400]);
        let clipping = channel_clipping(&capture, 0).expect("every sample on the rail");
        assert_eq!((clipping.at_top, clipping.at_bottom), (400, 0));
        assert_eq!(clipping.share(), 1.0);
        let said = clipping.describe();
        assert!(said.contains("every sample"), "{said}");
        assert!(said.contains("+2.50 V"), "{said}");
        assert!(said.contains("larger range"), "{said}");
    }

    #[test]
    fn a_signal_inside_the_range_is_not_clipped() {
        // Codes 40..200: nowhere near either end code.
        let codes: Vec<u8> = (0..400).map(|i| 40 + (i % 160) as u8).collect();
        assert!(channel_clipping(&clip_capture(codes), 0).is_none());
        // One stray sample on the rail is a signal that touched full scale,
        // not a capture that went past it.
        let mut codes: Vec<u8> = vec![128; 4000];
        codes[7] = 255;
        assert!(channel_clipping(&clip_capture(codes), 0).is_none());
    }

    #[test]
    fn both_rails_are_named_with_their_shares() {
        let mut codes = vec![128u8; 100];
        codes[..30].fill(255);
        codes[30..40].fill(0);
        let clipping = channel_clipping(&clip_capture(codes), 0).expect("clipped at both ends");
        assert_eq!((clipping.at_top, clipping.at_bottom), (30, 10));
        let said = clipping.describe();
        assert!(said.contains("30.0 %") && said.contains("10.0 %"), "{said}");
        assert!(
            said.contains("+2.50 V") && said.contains("-2.50 V"),
            "{said}"
        );
    }

    #[test]
    fn a_digital_capture_has_no_converter_to_clip() {
        let capture = Capture {
            id: crate::CaptureId::new(),
            meta: CaptureMeta {
                channels: vec![4],
                device: DeviceId::new(3, vec![3], 0x0925, 0x3881),
                rate_hz: 1_000_000,
                channel_count: 1,
                encoding: Encoding::PackedDigital,
                t0_ns: 0,
                trigger_position: None,
            },
            data: CaptureData::Digital {
                transitions: crate::capture_store::derive_transitions(&[0, 16, 0, 16]),
                packed: vec![0, 16, 0, 16],
            },
        };
        assert!(channel_clipping(&capture, 0).is_none());
        assert!(hardware_channel_clipping(&capture, 4).is_none());
    }

    /// One analog channel at the 2.5 V range: 255 codes over -2.5..+2.5 V,
    /// the calibration the oscilloscope reports there.
    fn clip_capture(raw: Vec<u8>) -> Capture {
        Capture {
            id: crate::CaptureId::new(),
            meta: CaptureMeta {
                channels: vec![0],
                device: DeviceId::new(3, vec![3, 4, 2], 0x1d50, 0x608e),
                rate_hz: 8_000_000,
                channel_count: 1,
                encoding: Encoding::U8Interleaved,
                t0_ns: 0,
                trigger_position: None,
            },
            data: CaptureData::Analog {
                raw,
                sample_bits: 8,
                channel_scale: vec![5.0 / 255.0],
                channel_offset: vec![-2.5],
            },
        }
    }
    use super::*;
    use crate::capture_store::CaptureMeta;
    use crate::{CaptureId, DeviceId};
    use awic_proto::frame::Encoding;

    fn meta(channel_count: u8) -> CaptureMeta {
        CaptureMeta {
            device: DeviceId::new(3, vec![1], 0, 0),
            rate_hz: 1_000_000,
            channel_count,
            channels: (0..channel_count).collect(),
            encoding: Encoding::PackedDigital,
            t0_ns: 0,
            trigger_position: None,
        }
    }

    #[test]
    fn analog_applies_the_captures_own_calibration_not_the_bit_depth() {
        let capture = Capture {
            id: CaptureId::new(),
            meta: meta(2),
            data: CaptureData::Analog {
                // Two channels interleaved: ch0 = 0, 2; ch1 = 1, 3.
                raw: vec![0, 1, 2, 3],
                sample_bits: 8,
                channel_scale: vec![0.5, 2.0],
                channel_offset: vec![-1.0, 10.0],
            },
        };
        assert_eq!(
            channel_series(&capture, 0).unwrap(),
            Series::Volts(vec![-1.0, 0.0])
        );
        assert_eq!(
            channel_series(&capture, 1).unwrap(),
            Series::Volts(vec![12.0, 16.0])
        );
        assert!(channel_series(&capture, 2).is_err(), "only two channels");
    }

    #[test]
    fn digital_addresses_the_bit_inside_the_packed_byte() {
        let capture = Capture {
            id: CaptureId::new(),
            meta: meta(2),
            data: CaptureData::Digital {
                packed: vec![0b01, 0b10, 0b11, 0b00],
                transitions: vec![],
            },
        };
        assert_eq!(
            channel_series(&capture, 0).unwrap(),
            Series::Levels(vec![true, false, true, false])
        );
        assert_eq!(
            channel_series(&capture, 1).unwrap(),
            Series::Levels(vec![false, true, true, false])
        );
        assert!(channel_series(&capture, 8).is_err(), "bits 0-7 only");
    }

    #[test]
    fn levels_become_zero_and_one_so_a_plot_needs_no_match() {
        let s = Series::Levels(vec![false, true]);
        assert_eq!(s.to_f64(), vec![0.0, 1.0]);
        assert_eq!(s.unit(), "");
        assert_eq!(Series::Volts(vec![1.5]).unit(), "V");
    }
}
