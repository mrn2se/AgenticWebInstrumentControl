//! Binary sample frame header codec (SPECIFICATION.md §3.2).
//!
//! ```text
//! off  size  field
//!  0    1    magic 0xA5
//!  1    1    version (1)
//!  2    2    stream_id        u16
//!  4    4    sequence         u32    per stream; detects drops
//!  8    1    encoding         u8     0=u8 interleaved, 1=i16, 2=packed digital, 3=minmax pairs
//!  9    1    channel_count    u8
//! 10    2    reserved
//! 12    4    sample_count     u32    per channel
//! 16    8    t0_ns            u64    capture-relative
//! 24   ..    payload
//! ```
//!
//! Sample data never travels as JSON (SPECIFICATION.md §3.2, DEVELOPMENT.md
//! §1 rule 5) — this header is the entire "schema" for the binary side of the
//! wire protocol.

/// First byte of every frame.
pub const MAGIC: u8 = 0xA5;
/// Only version currently defined.
pub const VERSION: u8 = 1;
/// Fixed header length in bytes.
pub const HEADER_LEN: usize = 24;

/// Sample encoding, carried in the header so a slow reader can tell a
/// full-resolution frame from a backpressure-decimated one without any
/// out-of-band signaling (SPECIFICATION.md §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Encoding {
    /// Raw `u8` samples, channels interleaved.
    U8Interleaved = 0,
    /// Raw little-endian `i16` samples, channels interleaved.
    I16 = 1,
    /// Digital channels packed one bit per channel per sample.
    PackedDigital = 2,
    /// Decimated `u8` analog: channel-major, per channel `buckets` x
    /// `(min: u8, max: u8)`. The backpressure / preview encoding for 8-bit
    /// analog sources.
    MinMaxU8 = 3,
    /// Decimated `i16` analog: channel-major, per channel `buckets` x
    /// `(min: i16 LE, max: i16 LE)`.
    MinMaxI16 = 4,
    /// Decimated packed digital: a single stream (not channel-major — one
    /// byte already carries every channel), `buckets` x `(AND: u8, OR: u8)`.
    /// Per channel bit: (0,0) low, (1,1) high, (0,1) toggled in the bucket.
    ///
    /// Distinct from the min/max encodings on purpose: a bitmask is not a
    /// number, and the frame must say which it is — a decoder cannot recover
    /// the width or the semantics of decimated data from anything else.
    BitwiseDigital = 5,
}

impl Encoding {
    /// True for the decimated (preview) encodings. These are a *view* of a
    /// capture, never a capture: nothing decimated may enter the capture
    /// store, which holds full resolution only (SPEC §2.7).
    pub const fn is_decimated(self) -> bool {
        matches!(
            self,
            Encoding::MinMaxU8 | Encoding::MinMaxI16 | Encoding::BitwiseDigital
        )
    }

    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Encoding::U8Interleaved),
            1 => Some(Encoding::I16),
            2 => Some(Encoding::PackedDigital),
            3 => Some(Encoding::MinMaxU8),
            4 => Some(Encoding::MinMaxI16),
            5 => Some(Encoding::BitwiseDigital),
            _ => None,
        }
    }
}

/// A decoded (or to-be-encoded) sample frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub stream_id: u16,
    /// Monotonically increasing per stream; a gap means dropped frames.
    pub sequence: u32,
    pub encoding: Encoding,
    pub channel_count: u8,
    /// Sample count *per channel* in the payload that follows.
    pub sample_count: u32,
    /// Capture-relative timestamp of the first sample, in nanoseconds.
    pub t0_ns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FrameHeaderError {
    #[error("frame buffer too short: need at least {need} bytes, got {got}")]
    TooShort { need: usize, got: usize },
    #[error("bad magic byte: expected 0x{expected:02X}, got 0x{actual:02X}")]
    BadMagic { expected: u8, actual: u8 },
    #[error("unsupported frame version: {0}")]
    UnsupportedVersion(u8),
    #[error("unknown encoding byte: {0}")]
    UnknownEncoding(u8),
}

/// Encode `header` into its fixed 24-byte little-endian wire form. Reserved
/// bytes are always zeroed.
pub fn encode_header(header: &FrameHeader) -> [u8; HEADER_LEN] {
    let mut buf = [0u8; HEADER_LEN];
    buf[0] = MAGIC;
    buf[1] = VERSION;
    buf[2..4].copy_from_slice(&header.stream_id.to_le_bytes());
    buf[4..8].copy_from_slice(&header.sequence.to_le_bytes());
    buf[8] = header.encoding.as_u8();
    buf[9] = header.channel_count;
    // buf[10..12] reserved, left zero.
    buf[12..16].copy_from_slice(&header.sample_count.to_le_bytes());
    buf[16..24].copy_from_slice(&header.t0_ns.to_le_bytes());
    buf
}

/// Decode a [`FrameHeader`] from the front of `buf`. `buf` may be longer than
/// [`HEADER_LEN`] (the payload follows); only the first 24 bytes are read.
pub fn decode_header(buf: &[u8]) -> Result<FrameHeader, FrameHeaderError> {
    if buf.len() < HEADER_LEN {
        return Err(FrameHeaderError::TooShort {
            need: HEADER_LEN,
            got: buf.len(),
        });
    }
    if buf[0] != MAGIC {
        return Err(FrameHeaderError::BadMagic {
            expected: MAGIC,
            actual: buf[0],
        });
    }
    if buf[1] != VERSION {
        return Err(FrameHeaderError::UnsupportedVersion(buf[1]));
    }

    let stream_id = u16::from_le_bytes([buf[2], buf[3]]);
    let sequence = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let encoding = Encoding::from_u8(buf[8]).ok_or(FrameHeaderError::UnknownEncoding(buf[8]))?;
    let channel_count = buf[9];
    let sample_count = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
    let t0_ns = u64::from_le_bytes(buf[16..24].try_into().expect("slice is 8 bytes"));

    Ok(FrameHeader {
        stream_id,
        sequence,
        encoding,
        channel_count,
        sample_count,
        t0_ns,
    })
}

/// Encode a full frame: header followed by `payload`, verbatim.
pub fn encode_frame(header: &FrameHeader, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_LEN + payload.len());
    buf.extend_from_slice(&encode_header(header));
    buf.extend_from_slice(payload);
    buf
}

/// Decode a full frame, returning the header and a slice over the remaining
/// payload bytes.
pub fn decode_frame(buf: &[u8]) -> Result<(FrameHeader, &[u8]), FrameHeaderError> {
    let header = decode_header(buf)?;
    Ok((header, &buf[HEADER_LEN..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn header_is_exactly_24_bytes() {
        let header = FrameHeader {
            stream_id: 1,
            sequence: 1,
            encoding: Encoding::I16,
            channel_count: 2,
            sample_count: 1024,
            t0_ns: 0,
        };
        assert_eq!(encode_header(&header).len(), HEADER_LEN);
    }

    #[test]
    fn round_trip_known_values() {
        let header = FrameHeader {
            stream_id: 7,
            sequence: 42,
            encoding: Encoding::MinMaxU8,
            channel_count: 8,
            sample_count: 1_048_576,
            t0_ns: 1_234_567_890_123,
        };
        let encoded = encode_header(&header);
        let decoded = decode_header(&encoded).unwrap();
        assert_eq!(decoded, header);
    }

    #[test]
    fn header_starts_with_magic_and_version() {
        let header = FrameHeader {
            stream_id: 0,
            sequence: 0,
            encoding: Encoding::U8Interleaved,
            channel_count: 1,
            sample_count: 0,
            t0_ns: 0,
        };
        let encoded = encode_header(&header);
        assert_eq!(encoded[0], 0xA5);
        assert_eq!(encoded[1], 1);
    }

    #[test]
    fn reserved_bytes_are_zeroed() {
        let header = FrameHeader {
            stream_id: 0xFFFF,
            sequence: 0xFFFF_FFFF,
            encoding: Encoding::PackedDigital,
            channel_count: 0xFF,
            sample_count: 0xFFFF_FFFF,
            t0_ns: u64::MAX,
        };
        let encoded = encode_header(&header);
        assert_eq!(&encoded[10..12], &[0, 0]);
    }

    #[test]
    fn rejects_short_buffer() {
        let err = decode_header(&[0xA5, 1, 0, 0]).unwrap_err();
        assert_eq!(
            err,
            FrameHeaderError::TooShort {
                need: HEADER_LEN,
                got: 4
            }
        );
    }

    #[test]
    fn rejects_bad_magic() {
        let mut buf = encode_header(&FrameHeader {
            stream_id: 0,
            sequence: 0,
            encoding: Encoding::I16,
            channel_count: 1,
            sample_count: 1,
            t0_ns: 0,
        });
        buf[0] = 0x00;
        let err = decode_header(&buf).unwrap_err();
        assert_eq!(
            err,
            FrameHeaderError::BadMagic {
                expected: 0xA5,
                actual: 0x00
            }
        );
    }

    #[test]
    fn rejects_unsupported_version() {
        let mut buf = encode_header(&FrameHeader {
            stream_id: 0,
            sequence: 0,
            encoding: Encoding::I16,
            channel_count: 1,
            sample_count: 1,
            t0_ns: 0,
        });
        buf[1] = 99;
        let err = decode_header(&buf).unwrap_err();
        assert_eq!(err, FrameHeaderError::UnsupportedVersion(99));
    }

    #[test]
    fn rejects_unknown_encoding() {
        let mut buf = encode_header(&FrameHeader {
            stream_id: 0,
            sequence: 0,
            encoding: Encoding::I16,
            channel_count: 1,
            sample_count: 1,
            t0_ns: 0,
        });
        buf[8] = 200;
        let err = decode_header(&buf).unwrap_err();
        assert_eq!(err, FrameHeaderError::UnknownEncoding(200));
    }

    #[test]
    fn frame_round_trip_carries_payload() {
        let header = FrameHeader {
            stream_id: 3,
            sequence: 5,
            encoding: Encoding::U8Interleaved,
            channel_count: 2,
            sample_count: 4,
            t0_ns: 99,
        };
        let payload = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let frame = encode_frame(&header, &payload);
        assert_eq!(frame.len(), HEADER_LEN + payload.len());

        let (decoded_header, decoded_payload) = decode_frame(&frame).unwrap();
        assert_eq!(decoded_header, header);
        assert_eq!(decoded_payload, &payload);
    }

    fn arb_encoding() -> impl Strategy<Value = Encoding> {
        prop_oneof![
            Just(Encoding::U8Interleaved),
            Just(Encoding::I16),
            Just(Encoding::PackedDigital),
            Just(Encoding::MinMaxU8),
            Just(Encoding::MinMaxI16),
            Just(Encoding::BitwiseDigital),
        ]
    }

    proptest! {
        #[test]
        fn prop_header_round_trips(
            stream_id: u16,
            sequence: u32,
            encoding in arb_encoding(),
            channel_count: u8,
            sample_count: u32,
            t0_ns: u64,
        ) {
            let header = FrameHeader { stream_id, sequence, encoding, channel_count, sample_count, t0_ns };
            let encoded = encode_header(&header);
            prop_assert_eq!(encoded.len(), HEADER_LEN);
            let decoded = decode_header(&encoded).unwrap();
            prop_assert_eq!(decoded, header);
        }

        #[test]
        fn prop_frame_round_trips(
            stream_id: u16,
            sequence: u32,
            encoding in arb_encoding(),
            channel_count: u8,
            sample_count: u32,
            t0_ns: u64,
            payload in proptest::collection::vec(any::<u8>(), 0..64),
        ) {
            let header = FrameHeader { stream_id, sequence, encoding, channel_count, sample_count, t0_ns };
            let frame = encode_frame(&header, &payload);
            let (decoded_header, decoded_payload) = decode_frame(&frame).unwrap();
            prop_assert_eq!(decoded_header, header);
            prop_assert_eq!(decoded_payload, payload.as_slice());
        }
    }
}
