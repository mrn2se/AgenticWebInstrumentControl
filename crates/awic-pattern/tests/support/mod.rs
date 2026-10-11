//! Shared fixtures for the integration tests.
//!
//! Each test binary only uses a subset of these — `mod support;` compiles
//! the whole file per binary, so unused items here are expected, not a
//! real dead-code issue.
#![allow(dead_code)]

use awic_pattern::{ChannelInfo, ChannelKind, RecordingInfo};

/// D0..D7 digital, plus CH1 (10 mV/code, 0-centred) and CH2 (inverted gain,
/// offset so code 128 = 0V) analog, matching what a real 8-channel LA +
/// 2-channel scope recording would report.
pub fn channels() -> Vec<ChannelInfo> {
    let mut v: Vec<ChannelInfo> = (0..8u8)
        .map(|bit| ChannelInfo {
            name: format!("D{bit}"),
            kind: ChannelKind::Digital { bit },
        })
        .collect();
    v.push(ChannelInfo {
        name: "CH1".into(),
        kind: ChannelKind::Analog {
            index: 0,
            scale: 0.01,
            offset: -1.28,
        },
    });
    v.push(ChannelInfo {
        name: "CH2".into(),
        kind: ChannelKind::Analog {
            index: 1,
            scale: -0.01,
            offset: 1.28,
        },
    });
    v
}

pub fn rec_1mhz() -> RecordingInfo {
    RecordingInfo {
        rate_hz: 1_000_000.0,
        sample_bits: 8,
        channel_count: 2,
    }
}

/// Same as [`rec_1mhz`] but a single analog channel, for tests that feed a
/// plain (non-interleaved) code array to `feed_analog`.
pub fn rec_1mhz_1ch() -> RecordingInfo {
    RecordingInfo {
        rate_hz: 1_000_000.0,
        sample_bits: 8,
        channel_count: 1,
    }
}
