//! Binary view frame header codec (SPECIFICATION.md §3.4).
//!
//! View frames are the recorder's answer to `rec.view`/`rec.view.update`: a
//! placeable window into a recording's history, distinct from the §3.2
//! sample frame both in wire format (its own magic, so a client dispatches
//! on the first byte) and in what it can express — `first_sample` and
//! `samples_per_item` give every item an exact time span on its own,
//! closing the gap that decimated §3.2 stream frames leave (they carry no
//! span of their own).
//!
//! ```text
//! off  size  field
//!  0    1    magic 0xA6
//!  1    1    version (1)
//!  2    2    view_id          u16
//!  4    4    sequence         u32    per view
//!  8    1    kind             u8     1=digital runs, 2=digital summary, 3=analog raw, 4=analog summary
//!  9    1    channel_count    u8
//! 10    1    sample_bits      u8     8 or 16 (analog); 8 (digital)
//! 11    1    flags            u8     bit 0: follow view; bit 1: range truncated by eviction
//! 12    4    item_count       u32
//! 16    8    first_sample     u64    absolute sample index of the first item
//! 24    8    samples_per_item u64    1 for runs and raw samples
//! 32   ..    payload
//! ```

/// First byte of every view frame.
pub const MAGIC: u8 = 0xA6;
/// Only version currently defined.
pub const VERSION: u8 = 1;
/// Fixed header length in bytes.
pub const HEADER_LEN: usize = 32;

/// The `kind` byte, selecting the payload layout (SPEC §3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ViewFrameKind {
    /// `item_count` x (`value: u8`, `length: LEB128`) — exact edges.
    DigitalRuns = 1,
    /// `item_count` x (`AND: u8`, `OR: u8`).
    DigitalSummary = 2,
    /// Raw codes, channels interleaved, `sample_bits` wide, little-endian.
    AnalogRaw = 3,
    /// Channel-major; per channel `item_count` x (`min`, `max`), `sample_bits` wide.
    AnalogSummary = 4,
}

impl ViewFrameKind {
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(ViewFrameKind::DigitalRuns),
            2 => Some(ViewFrameKind::DigitalSummary),
            3 => Some(ViewFrameKind::AnalogRaw),
            4 => Some(ViewFrameKind::AnalogSummary),
            _ => None,
        }
    }
}

/// The `flags` byte: independent bits, not a "which encoding" tag like
/// [`ViewFrameKind`] — a frame can be both a follow view and truncated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ViewFrameFlags {
    /// Bit 0: this is a `follow_s` live-tail view, resent at up to `fps`.
    pub follow: bool,
    /// Bit 1: the requested range started before the recording's
    /// `first_sample` and was clipped to the retained span.
    pub truncated: bool,
}

impl ViewFrameFlags {
    pub const fn as_u8(self) -> u8 {
        (self.follow as u8) | ((self.truncated as u8) << 1)
    }

    pub const fn from_u8(v: u8) -> Self {
        ViewFrameFlags {
            follow: v & 0b01 != 0,
            truncated: v & 0b10 != 0,
        }
    }
}

/// A decoded (or to-be-encoded) view frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewFrameHeader {
    pub view_id: u16,
    /// Monotonically increasing per view; a gap means a conflated
    /// (dropped, never queued — SPEC §3.4) follow-view tick.
    pub sequence: u32,
    pub kind: ViewFrameKind,
    pub channel_count: u8,
    /// 8 or 16 for analog; 8 for digital.
    pub sample_bits: u8,
    pub flags: ViewFrameFlags,
    pub item_count: u32,
    /// Absolute sample index of the first item.
    pub first_sample: u64,
    /// 1 for [`ViewFrameKind::DigitalRuns`] and [`ViewFrameKind::AnalogRaw`];
    /// a power of two otherwise, aligned so item boundaries are stable
    /// while a window scrolls (SPEC §3.4).
    pub samples_per_item: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ViewFrameHeaderError {
    #[error("view frame buffer too short: need at least {need} bytes, got {got}")]
    TooShort { need: usize, got: usize },
    #[error("bad magic byte: expected 0x{expected:02X}, got 0x{actual:02X}")]
    BadMagic { expected: u8, actual: u8 },
    #[error("unsupported view frame version: {0}")]
    UnsupportedVersion(u8),
    #[error("unknown view frame kind: {0}")]
    UnknownKind(u8),
}

/// Encode `header` into its fixed 32-byte little-endian wire form.
pub fn encode_view_frame_header(header: &ViewFrameHeader) -> [u8; HEADER_LEN] {
    let mut buf = [0u8; HEADER_LEN];
    buf[0] = MAGIC;
    buf[1] = VERSION;
    buf[2..4].copy_from_slice(&header.view_id.to_le_bytes());
    buf[4..8].copy_from_slice(&header.sequence.to_le_bytes());
    buf[8] = header.kind.as_u8();
    buf[9] = header.channel_count;
    buf[10] = header.sample_bits;
    buf[11] = header.flags.as_u8();
    buf[12..16].copy_from_slice(&header.item_count.to_le_bytes());
    buf[16..24].copy_from_slice(&header.first_sample.to_le_bytes());
    buf[24..32].copy_from_slice(&header.samples_per_item.to_le_bytes());
    buf
}

/// Decode a [`ViewFrameHeader`] from the front of `buf`. `buf` may be longer
/// than [`HEADER_LEN`] (the payload follows); only the first 32 bytes are
/// read.
pub fn decode_view_frame_header(buf: &[u8]) -> Result<ViewFrameHeader, ViewFrameHeaderError> {
    if buf.len() < HEADER_LEN {
        return Err(ViewFrameHeaderError::TooShort {
            need: HEADER_LEN,
            got: buf.len(),
        });
    }
    if buf[0] != MAGIC {
        return Err(ViewFrameHeaderError::BadMagic {
            expected: MAGIC,
            actual: buf[0],
        });
    }
    if buf[1] != VERSION {
        return Err(ViewFrameHeaderError::UnsupportedVersion(buf[1]));
    }

    let view_id = u16::from_le_bytes([buf[2], buf[3]]);
    let sequence = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let kind = ViewFrameKind::from_u8(buf[8]).ok_or(ViewFrameHeaderError::UnknownKind(buf[8]))?;
    let channel_count = buf[9];
    let sample_bits = buf[10];
    let flags = ViewFrameFlags::from_u8(buf[11]);
    let item_count = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
    let first_sample = u64::from_le_bytes(buf[16..24].try_into().expect("slice is 8 bytes"));
    let samples_per_item = u64::from_le_bytes(buf[24..32].try_into().expect("slice is 8 bytes"));

    Ok(ViewFrameHeader {
        view_id,
        sequence,
        kind,
        channel_count,
        sample_bits,
        flags,
        item_count,
        first_sample,
        samples_per_item,
    })
}

/// Encode a full view frame: header followed by `payload`, verbatim.
pub fn encode_view_frame(header: &ViewFrameHeader, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_LEN + payload.len());
    buf.extend_from_slice(&encode_view_frame_header(header));
    buf.extend_from_slice(payload);
    buf
}

/// Decode a full view frame, returning the header and a slice over the
/// remaining payload bytes.
pub fn decode_view_frame(buf: &[u8]) -> Result<(ViewFrameHeader, &[u8]), ViewFrameHeaderError> {
    let header = decode_view_frame_header(buf)?;
    Ok((header, &buf[HEADER_LEN..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn sample_header() -> ViewFrameHeader {
        ViewFrameHeader {
            view_id: 4,
            sequence: 1,
            kind: ViewFrameKind::DigitalSummary,
            channel_count: 8,
            sample_bits: 8,
            flags: ViewFrameFlags {
                follow: true,
                truncated: false,
            },
            item_count: 1600,
            first_sample: 3_100_000,
            samples_per_item: 1024,
        }
    }

    #[test]
    fn header_is_exactly_32_bytes() {
        assert_eq!(encode_view_frame_header(&sample_header()).len(), HEADER_LEN);
    }

    #[test]
    fn round_trip_known_values() {
        let header = sample_header();
        let encoded = encode_view_frame_header(&header);
        let decoded = decode_view_frame_header(&encoded).unwrap();
        assert_eq!(decoded, header);
    }

    #[test]
    fn header_starts_with_magic_and_version() {
        let encoded = encode_view_frame_header(&sample_header());
        assert_eq!(encoded[0], 0xA6);
        assert_eq!(encoded[1], 1);
    }

    #[test]
    fn flags_round_trip_independently() {
        for (follow, truncated) in [(false, false), (true, false), (false, true), (true, true)] {
            let flags = ViewFrameFlags { follow, truncated };
            assert_eq!(ViewFrameFlags::from_u8(flags.as_u8()), flags);
        }
    }

    #[test]
    fn rejects_short_buffer() {
        let err = decode_view_frame_header(&[0xA6, 1, 0, 0]).unwrap_err();
        assert_eq!(
            err,
            ViewFrameHeaderError::TooShort {
                need: HEADER_LEN,
                got: 4
            }
        );
    }

    #[test]
    fn rejects_bad_magic() {
        let mut buf = encode_view_frame_header(&sample_header());
        buf[0] = 0x00;
        let err = decode_view_frame_header(&buf).unwrap_err();
        assert_eq!(
            err,
            ViewFrameHeaderError::BadMagic {
                expected: 0xA6,
                actual: 0x00
            }
        );
    }

    #[test]
    fn rejects_unsupported_version() {
        let mut buf = encode_view_frame_header(&sample_header());
        buf[1] = 99;
        let err = decode_view_frame_header(&buf).unwrap_err();
        assert_eq!(err, ViewFrameHeaderError::UnsupportedVersion(99));
    }

    #[test]
    fn rejects_unknown_kind() {
        let mut buf = encode_view_frame_header(&sample_header());
        buf[8] = 0;
        let err = decode_view_frame_header(&buf).unwrap_err();
        assert_eq!(err, ViewFrameHeaderError::UnknownKind(0));

        let mut buf2 = encode_view_frame_header(&sample_header());
        buf2[8] = 5;
        let err2 = decode_view_frame_header(&buf2).unwrap_err();
        assert_eq!(err2, ViewFrameHeaderError::UnknownKind(5));
    }

    #[test]
    fn frame_round_trip_carries_payload() {
        let header = sample_header();
        let payload = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let frame = encode_view_frame(&header, &payload);
        assert_eq!(frame.len(), HEADER_LEN + payload.len());

        let (decoded_header, decoded_payload) = decode_view_frame(&frame).unwrap();
        assert_eq!(decoded_header, header);
        assert_eq!(decoded_payload, &payload);
    }

    fn arb_kind() -> impl Strategy<Value = ViewFrameKind> {
        prop_oneof![
            Just(ViewFrameKind::DigitalRuns),
            Just(ViewFrameKind::DigitalSummary),
            Just(ViewFrameKind::AnalogRaw),
            Just(ViewFrameKind::AnalogSummary),
        ]
    }

    proptest! {
        #[test]
        fn prop_header_round_trips(
            view_id: u16,
            sequence: u32,
            kind in arb_kind(),
            channel_count: u8,
            sample_bits: u8,
            flags_byte in 0u8..4,
            item_count: u32,
            first_sample: u64,
            samples_per_item: u64,
        ) {
            let header = ViewFrameHeader {
                view_id, sequence, kind, channel_count, sample_bits,
                flags: ViewFrameFlags::from_u8(flags_byte),
                item_count, first_sample, samples_per_item,
            };
            let encoded = encode_view_frame_header(&header);
            prop_assert_eq!(encoded.len(), HEADER_LEN);
            let decoded = decode_view_frame_header(&encoded).unwrap();
            prop_assert_eq!(decoded, header);
        }

        #[test]
        fn prop_frame_round_trips(
            view_id: u16,
            sequence: u32,
            kind in arb_kind(),
            channel_count: u8,
            sample_bits: u8,
            flags_byte in 0u8..4,
            item_count: u32,
            first_sample: u64,
            samples_per_item: u64,
            payload in proptest::collection::vec(any::<u8>(), 0..64),
        ) {
            let header = ViewFrameHeader {
                view_id, sequence, kind, channel_count, sample_bits,
                flags: ViewFrameFlags::from_u8(flags_byte),
                item_count, first_sample, samples_per_item,
            };
            let frame = encode_view_frame(&header, &payload);
            let (decoded_header, decoded_payload) = decode_view_frame(&frame).unwrap();
            prop_assert_eq!(decoded_header, header);
            prop_assert_eq!(decoded_payload, payload.as_slice());
        }
    }
}
