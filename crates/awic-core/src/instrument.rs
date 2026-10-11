//! The backend abstraction (SPECIFICATION.md §2.2, §2.3, §2.6).
//!
//! `awic-core` never names a driver-specific type here or anywhere else in
//! its public API — that is the whole point of the split: `awic-native` and
//! `awic-fake` both implement [`Backend`] (and construct [`Instrument`]s)
//! without core knowing either exists.

use async_trait::async_trait;
use awic_proto::frame::Encoding;
use bytes::Bytes;
use tokio::sync::mpsc;

use crate::capture_store::CaptureId;
use crate::device_id::DeviceId;
use crate::error::Result;
use crate::Capability;

/// Acquisition configuration and trigger, shared verbatim with the wire
/// protocol (SPECIFICATION.md §3.1) — see `awic-proto`'s `control` module for
/// the type definitions and their `ts-rs` bindings.
pub use awic_proto::{AcqConfig, ChannelCalibration, Slope, Trigger};

/// What an [`Instrument`] will actually do after [`Instrument::configure`].
///
/// Returned rather than assumed because the device, not the caller, decides:
/// the FX2 oscilloscope supports only a fixed rate ladder, so a request for
/// 10 MHz runs at a neighbouring rate — and a caller that kept its own
/// requested value would build a wrong timebase. Likewise only the driver
/// knows each range's gain and calibration offset.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveAcq {
    /// The configuration as applied: rate, channels, ranges as the hardware
    /// will run them.
    pub cfg: AcqConfig,
    /// One entry per enabled channel, in `cfg.channels` order.
    pub calibration: Vec<ChannelCalibration>,
}

impl EffectiveAcq {
    /// For digital instruments that apply the request exactly: logic-level
    /// calibration for every enabled channel.
    pub fn digital(cfg: AcqConfig) -> Self {
        let calibration = cfg
            .channels
            .iter()
            .map(|&ch| ChannelCalibration::digital(ch))
            .collect();
        Self { cfg, calibration }
    }
}

/// One chunk of raw samples pushed by an [`Instrument`] while acquiring.
///
/// The payload layout matches the binary wire frame's payload
/// (SPECIFICATION.md §3.2) for the given `encoding`, so a transport can wrap
/// it in a frame header and send it on with no reshaping.
#[derive(Debug, Clone)]
pub struct SampleChunk {
    pub encoding: Encoding,
    pub channel_count: u8,
    /// Sample count *per channel* in `data`.
    pub sample_count: u32,
    /// Capture-relative timestamp of the first sample, in nanoseconds.
    pub t0_ns: u64,
    pub data: Bytes,
}

/// Error returned when the receiving end of a [`SampleSink`] has gone away
/// (the capture was cancelled or the stream torn down).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("sample sink closed: receiver dropped")]
pub struct SinkClosed;

/// The write end of the bounded channel an [`Instrument`] streams samples
/// into. Bounded so a slow consumer applies backpressure to the producer
/// rather than growing memory without limit (SPECIFICATION.md §2.6); turning
/// backpressure into a lower-resolution live view instead of a stall is a
/// transport-level policy (§3.2) built on top of this channel plus
/// [`crate::decimate::minmax_decimate`].
#[derive(Clone)]
pub struct SampleSink {
    tx: mpsc::Sender<SampleChunk>,
}

impl SampleSink {
    /// Creates a bounded sink/stream pair. `capacity` is the number of
    /// in-flight [`SampleChunk`]s before `send` starts to block.
    pub fn channel(capacity: usize) -> (SampleSink, mpsc::Receiver<SampleChunk>) {
        let (tx, rx) = mpsc::channel(capacity);
        (SampleSink { tx }, rx)
    }

    /// Sends a chunk, waiting for room if the channel is full.
    pub async fn send(&self, chunk: SampleChunk) -> std::result::Result<(), SinkClosed> {
        self.tx.send(chunk).await.map_err(|_| SinkClosed)
    }

    /// Sends a chunk only if there is room right now, so a driver's hot loop
    /// can choose to drop rather than stall — never silently applied by the
    /// sink itself (SPECIFICATION.md §3.2 requires an explicit encoding
    /// switch, not silent drops, above the high-water mark).
    pub fn try_send(
        &self,
        chunk: SampleChunk,
    ) -> std::result::Result<(), mpsc::error::TrySendError<SampleChunk>> {
        self.tx.try_send(chunk)
    }
}

/// A live, owned instrument (SPECIFICATION.md §2.3). One async task owns each
/// `Instrument`; commands reach it over an `mpsc`, samples leave over a
/// [`SampleSink`] (§2.6).
///
/// Deliberately **no `set_mode()`**: the oscilloscope's scope/logic selection is a
/// physical switch (§5.1), so there is nothing for software to change.
#[async_trait]
pub trait Instrument: Send + Sync {
    fn id(&self) -> &DeviceId;
    fn capabilities(&self) -> &[Capability];

    /// What this instrument is, in the words a person would use — "FX2 2-channel
    /// oscilloscope", not a VID:PID. Shown by `device.list`/
    /// `list_instruments`, where a bare bus path tells an agent (or a user)
    /// nothing about which box it is holding (live test B-07).
    fn label(&self) -> Option<String> {
        None
    }

    /// Whether `configure` arms `cfg.trigger` itself, delivering a capture
    /// whose edge is at `samples / 2`. When false — every real driver today —
    /// the server runs a software trigger over a continuous stream instead
    /// (live test B-32): a trigger is never accepted and then ignored.
    fn honours_trigger(&self) -> bool {
        false
    }

    /// Runs on **every** enumeration, not just the first. FX2 devices hold
    /// firmware in RAM and lose it on power loss, so re-preparing on each
    /// appearance is required, not an optimization to skip.
    async fn prepare(&mut self) -> Result<()>;

    /// Whether the next [`Self::configure`] will re-flash the device before it
    /// can run - an FX2 that stalled part-way through the last acquisition
    /// needs its firmware uploaded again, which costs a re-enumeration of a
    /// few seconds. The server marks such a device `flashing` while it happens
    /// instead of reporting it `ready` throughout (B-63); a driver with no
    /// such state says `false` and nothing changes for it.
    fn pending_reset(&self) -> bool {
        false
    }

    /// Validates and applies `cfg`, returning what the device will actually
    /// do. Callers must use the returned configuration and calibration, not
    /// their request (see [`EffectiveAcq`]).
    async fn configure(&mut self, cfg: AcqConfig) -> Result<EffectiveAcq>;

    /// Starts acquisition, streaming samples into `sink` until [`Self::stop`]
    /// is called or the capture completes. Returns the id under which the
    /// resulting capture will be addressable in the `CaptureStore`.
    async fn start(&mut self, sink: SampleSink) -> Result<CaptureId>;

    async fn stop(&mut self) -> Result<()>;
}

/// A source of instruments (SPECIFICATION.md §2.2). `awic-native` (real FX2
/// drivers) and `awic-fake` (synthetic signals) both implement this; a third
/// backend can be added without touching `awic-core` — that property is the
/// test of the abstraction (§2.2).
#[async_trait]
pub trait Backend: Send + Sync {
    /// Human-readable name for logs/diagnostics, e.g. `"native"`, `"fake"`.
    fn name(&self) -> &str;

    /// Enumerate instruments this backend currently knows about.
    async fn discover(&self) -> Result<Vec<DeviceId>>;

    /// Open (or re-open) the instrument at `id`. Called by the `Registry`
    /// after a device is detected, and again after every re-enumeration
    /// (§4.5).
    async fn open(&self, id: &DeviceId) -> Result<Box<dyn Instrument>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sample_sink_delivers_chunks_in_order() {
        let (sink, mut rx) = SampleSink::channel(4);
        for seq in 0..3u32 {
            sink.send(SampleChunk {
                encoding: Encoding::I16,
                channel_count: 1,
                sample_count: 10,
                t0_ns: seq as u64,
                data: Bytes::new(),
            })
            .await
            .unwrap();
        }
        drop(sink);

        let mut seen = Vec::new();
        while let Some(chunk) = rx.recv().await {
            seen.push(chunk.t0_ns);
        }
        assert_eq!(seen, vec![0, 1, 2]);
    }

    #[tokio::test]
    async fn sample_sink_send_errors_once_receiver_dropped() {
        let (sink, rx) = SampleSink::channel(1);
        drop(rx);
        let err = sink
            .send(SampleChunk {
                encoding: Encoding::U8Interleaved,
                channel_count: 1,
                sample_count: 1,
                t0_ns: 0,
                data: Bytes::new(),
            })
            .await
            .unwrap_err();
        assert_eq!(err, SinkClosed);
    }
}
