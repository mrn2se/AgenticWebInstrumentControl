//! Captures recorded on the real bench, for tests (feature `fixtures`).
//!
//! Synthetic signals test what their author thought of; a capture of the
//! actual SSD1306 bus or the oscilloscope's CAL output tests what the hardware
//! does — clock stretching, ACK hand-over spikes, a range-dependent zero
//! offset. Each fixture is two files in `testdata/bench/`: `<name>.json`
//! (what was captured, from which device, with which calibration, and what
//! the bench was wired to) and `<name>.bin` (the raw samples exactly as the
//! server's capture store holds them: packed digital bytes, or interleaved
//! `u8`/`i16` analog codes). `crates/awic-server/examples/record_fixture.rs`
//! records new ones from a running server.

use std::path::{Path, PathBuf};

use awic_proto::frame::Encoding;
use serde::{Deserialize, Serialize};

use crate::capture_store::{derive_transitions, Capture, CaptureData, CaptureMeta};
use crate::{CaptureId, ChannelCalibration, DeviceId};

/// `<name>.json`: everything needed to rebuild the capture, plus enough
/// about the bench that a failing test can be read without the hardware.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FixtureMeta {
    pub name: String,
    /// What is on the wires, in a sentence or two.
    pub description: String,
    /// When it was recorded (ISO date).
    pub recorded: String,
    pub device: String,
    pub device_label: String,
    pub rate_hz: u64,
    /// Hardware channels, in payload order.
    pub channels: Vec<u8>,
    /// `packed_digital`, `u8_interleaved` or `i16`.
    pub encoding: String,
    /// The driver's per-channel calibration, as `acq.configure` reported it.
    pub calibration: Vec<ChannelCalibration>,
    /// Samples per channel.
    pub samples: u64,
    #[serde(default)]
    pub trigger_position: Option<u64>,
    /// The trigger as configured, for reference.
    #[serde(default)]
    pub trigger: Option<serde_json::Value>,
}

/// `testdata/bench/` at the workspace root.
pub fn bench_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/bench")
}

/// Loads fixture `name` from [`bench_dir`] as a [`Capture`], as the capture
/// store would hold it. Panics with the path on any problem: this is for
/// tests, where a missing or malformed fixture should stop the test loudly.
pub fn load(name: &str) -> (FixtureMeta, Capture) {
    let dir = bench_dir();
    let json_path = dir.join(format!("{name}.json"));
    let meta: FixtureMeta = serde_json::from_slice(
        &std::fs::read(&json_path).unwrap_or_else(|e| panic!("{}: {e}", json_path.display())),
    )
    .unwrap_or_else(|e| panic!("{}: {e}", json_path.display()));
    let bin_path = dir.join(format!("{name}.bin"));
    let raw = std::fs::read(&bin_path).unwrap_or_else(|e| panic!("{}: {e}", bin_path.display()));
    let capture = to_capture(&meta, raw);
    (meta, capture)
}

/// Rebuilds the capture from its parts; checks the byte count against the
/// declared shape so a truncated `.bin` cannot pass for a short capture.
pub fn to_capture(meta: &FixtureMeta, raw: Vec<u8>) -> Capture {
    let channel_count = meta.channels.len() as u8;
    let (encoding, bytes_per_frame) = match meta.encoding.as_str() {
        "packed_digital" => (Encoding::PackedDigital, 1),
        "u8_interleaved" => (Encoding::U8Interleaved, usize::from(channel_count)),
        "i16" => (Encoding::I16, 2 * usize::from(channel_count)),
        other => panic!("fixture {}: unknown encoding {other:?}", meta.name),
    };
    assert_eq!(
        raw.len() as u64,
        meta.samples * bytes_per_frame as u64,
        "fixture {}: .bin holds {} bytes, the .json declares {} samples of {bytes_per_frame}",
        meta.name,
        raw.len(),
        meta.samples
    );
    let device: DeviceId = meta
        .device
        .parse()
        .unwrap_or_else(|e| panic!("fixture {}: device {:?}: {e}", meta.name, meta.device));
    let data = match encoding {
        Encoding::PackedDigital => CaptureData::Digital {
            transitions: derive_transitions(&raw),
            packed: raw,
        },
        _ => CaptureData::Analog {
            sample_bits: if encoding == Encoding::I16 { 16 } else { 8 },
            channel_scale: meta.calibration.iter().map(|c| c.scale).collect(),
            channel_offset: meta.calibration.iter().map(|c| c.offset).collect(),
            raw,
        },
    };
    Capture {
        id: CaptureId::new(),
        meta: CaptureMeta {
            device,
            rate_hz: meta.rate_hz,
            channel_count,
            channels: meta.channels.clone(),
            encoding,
            t0_ns: 0,
            trigger_position: meta.trigger_position,
        },
        data,
    }
}
