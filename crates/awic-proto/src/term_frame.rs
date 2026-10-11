//! Binary terminal-output frame codec (SPECIFICATION.md §3.5, §11.2).
//!
//! Terminal *input* (a browser typing into a serial session) travels as JSON
//! (`term.write`'s `data`/`data_b64` — control-plane traffic, small and
//! infrequent). Terminal *output* — a UART streaming at line rate, or the
//! server's own log — goes the other way over the same binary channel that
//! carries sample frames, distinguished by its own magic byte so the client
//! can dispatch on the first byte without the two ever being confused
//! (SPEC §3.2's sample frames use `0xA5`; this uses `0xA7`).
//!
//! ```text
//! off  size  field
//!  0    1    magic 0xA7
//!  1    1    version (1)
//!  2    2    session          u16
//!  4    4    sequence         u32    per session; detects drops
//!  8   ..    payload bytes
//! ```
//!
//! No `byte_len` field: unlike the sample-frame header (§3.2), which the
//! decoder must parse standalone from a buffer that may hold more than one
//! frame's worth of bytes, a terminal frame is always exactly one WebSocket
//! binary message — the transport already carries the length, so repeating
//! it here would be a redundant field to keep in sync for no benefit.
//!
//! A dropped chunk (this session's per-connection queue was full — SPEC
//! §3.5's drop-and-count policy) is not a distinct wire signal: the bytes
//! are simply gone, and the next frame that *does* get through carries a
//! `[<n> bytes dropped]` marker prepended to its payload, in-band, exactly
//! as a human reading raw serial output would want it. `sequence` still
//! advances across the gap, so a client can also detect the drop
//! structurally, the same way sample frames do.

/// First byte of every terminal output frame.
pub const MAGIC: u8 = 0xA7;
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 8;

/// A decoded (or to-be-encoded) terminal frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermFrameHeader {
    pub session_id: u16,
    /// Monotonically increasing per session; a gap means dropped output.
    pub sequence: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TermFrameError {
    #[error("frame buffer too short: need at least {need} bytes, got {got}")]
    TooShort { need: usize, got: usize },
    #[error("bad magic byte: expected 0x{expected:02X}, got 0x{actual:02X}")]
    BadMagic { expected: u8, actual: u8 },
    #[error("unsupported frame version: {0}")]
    UnsupportedVersion(u8),
}

/// Encodes `header` into its fixed 8-byte little-endian wire form.
pub fn encode_header(header: &TermFrameHeader) -> [u8; HEADER_LEN] {
    let mut buf = [0u8; HEADER_LEN];
    buf[0] = MAGIC;
    buf[1] = VERSION;
    buf[2..4].copy_from_slice(&header.session_id.to_le_bytes());
    buf[4..8].copy_from_slice(&header.sequence.to_le_bytes());
    buf
}

/// Decodes a [`TermFrameHeader`] from the front of `buf`. `buf` may be
/// longer than [`HEADER_LEN`] (the payload follows).
pub fn decode_header(buf: &[u8]) -> Result<TermFrameHeader, TermFrameError> {
    if buf.len() < HEADER_LEN {
        return Err(TermFrameError::TooShort {
            need: HEADER_LEN,
            got: buf.len(),
        });
    }
    if buf[0] != MAGIC {
        return Err(TermFrameError::BadMagic {
            expected: MAGIC,
            actual: buf[0],
        });
    }
    if buf[1] != VERSION {
        return Err(TermFrameError::UnsupportedVersion(buf[1]));
    }
    let session_id = u16::from_le_bytes([buf[2], buf[3]]);
    let sequence = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    Ok(TermFrameHeader {
        session_id,
        sequence,
    })
}

/// Encodes a full frame: header then `payload` verbatim.
pub fn encode_frame(session_id: u16, sequence: u32, payload: &[u8]) -> Vec<u8> {
    let header = TermFrameHeader {
        session_id,
        sequence,
    };
    let mut buf = Vec::with_capacity(HEADER_LEN + payload.len());
    buf.extend_from_slice(&encode_header(&header));
    buf.extend_from_slice(payload);
    buf
}

/// Decodes a full frame, returning the header and a slice over the
/// remaining payload bytes — the rest of the WebSocket message.
pub fn decode_frame(buf: &[u8]) -> Result<(TermFrameHeader, &[u8]), TermFrameError> {
    let header = decode_header(buf)?;
    Ok((header, &buf[HEADER_LEN..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn header_is_exactly_8_bytes() {
        let header = TermFrameHeader {
            session_id: 1,
            sequence: 1,
        };
        assert_eq!(encode_header(&header).len(), HEADER_LEN);
    }

    #[test]
    fn header_starts_with_magic_and_version() {
        let bytes = encode_header(&TermFrameHeader {
            session_id: 0,
            sequence: 0,
        });
        assert_eq!(bytes[0], 0xA7);
        assert_eq!(bytes[1], 1);
    }

    #[test]
    fn round_trip_known_values() {
        let header = TermFrameHeader {
            session_id: 7,
            sequence: 42,
        };
        let encoded = encode_header(&header);
        assert_eq!(decode_header(&encoded).unwrap(), header);
    }

    #[test]
    fn frame_round_trip_carries_payload() {
        let payload = b"hello uart\n";
        let frame = encode_frame(3, 9, payload);
        let (header, decoded_payload) = decode_frame(&frame).unwrap();
        assert_eq!(header.session_id, 3);
        assert_eq!(header.sequence, 9);
        assert_eq!(decoded_payload, payload);
    }

    #[test]
    fn rejects_short_buffer() {
        let err = decode_header(&[0xA7, 1, 0, 0]).unwrap_err();
        assert_eq!(
            err,
            TermFrameError::TooShort {
                need: HEADER_LEN,
                got: 4
            }
        );
    }

    #[test]
    fn rejects_bad_magic() {
        let mut buf = encode_frame(0, 0, b"x");
        buf[0] = 0xA5; // a sample frame, not a terminal frame
        let err = decode_header(&buf).unwrap_err();
        assert_eq!(
            err,
            TermFrameError::BadMagic {
                expected: 0xA7,
                actual: 0xA5
            }
        );
    }

    #[test]
    fn rejects_unsupported_version() {
        let mut buf = encode_frame(0, 0, b"x");
        buf[1] = 99;
        let err = decode_header(&buf).unwrap_err();
        assert_eq!(err, TermFrameError::UnsupportedVersion(99));
    }

    proptest! {
        #[test]
        fn prop_frame_round_trips(
            session_id: u16,
            sequence: u32,
            payload in proptest::collection::vec(any::<u8>(), 0..256),
        ) {
            let frame = encode_frame(session_id, sequence, &payload);
            let (header, decoded_payload) = decode_frame(&frame).unwrap();
            prop_assert_eq!(header.session_id, session_id);
            prop_assert_eq!(header.sequence, sequence);
            prop_assert_eq!(decoded_payload, payload.as_slice());
        }
    }
}
