//! A minimal test-only `Backend`/`Instrument` pair (SPECIFICATION.md §2.2,
//! §2.3), standing in for `awic-fake`/`awic-usb` for this crate's own
//! integration tests only — this crate's server-side code never depends on
//! either concrete crate, only on `awic-core`'s traits (see `src/lib.rs`).
//!
//! One instrument, one device (`"1-1"`), two analog channels. `start()`
//! streams `cfg.samples` samples in fixed-size chunks at a fast pace (no
//! real timing fidelity needed — these tests only care about the wire
//! protocol, not signal fidelity, which is `awic-fake`'s job).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use awic_core::{
    AcqConfig, Backend, Capability, CaptureId, DeviceId, Instrument, Result as CoreResult,
    SampleChunk, SampleSink,
};
use awic_proto::frame::Encoding;
use bytes::Bytes;

/// The one device [`TestBackend`] reports.
pub const TEST_DEVICE: &str = "1-1";

pub struct TestBackend {
    device_id: DeviceId,
}

impl TestBackend {
    pub fn new() -> Self {
        Self {
            device_id: TEST_DEVICE.parse().expect("valid device id"),
        }
    }
}

impl Default for TestBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Backend for TestBackend {
    fn name(&self) -> &str {
        "test"
    }

    async fn discover(&self) -> CoreResult<Vec<DeviceId>> {
        Ok(vec![self.device_id.clone()])
    }

    async fn open(&self, id: &DeviceId) -> CoreResult<Box<dyn Instrument>> {
        Ok(Box::new(TestInstrument::new(id.clone())))
    }
}

pub struct TestInstrument {
    id: DeviceId,
    capabilities: Vec<Capability>,
    cfg: Option<AcqConfig>,
    stop_flag: Arc<AtomicBool>,
}

impl TestInstrument {
    pub fn new(id: DeviceId) -> Self {
        Self {
            id,
            capabilities: vec![Capability::Analog {
                channels: 2,
                bits: 8,
                rates_hz: vec![1_000_000],
                ranges_v: vec![5.0],
            }],
            cfg: None,
            stop_flag: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[async_trait]
impl Instrument for TestInstrument {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn capabilities(&self) -> &[Capability] {
        &self.capabilities
    }

    async fn prepare(&mut self) -> CoreResult<()> {
        Ok(())
    }

    async fn configure(&mut self, cfg: AcqConfig) -> CoreResult<awic_core::EffectiveAcq> {
        self.cfg = Some(cfg.clone());
        Ok(awic_core::EffectiveAcq::digital(cfg))
    }

    async fn start(&mut self, sink: SampleSink) -> CoreResult<CaptureId> {
        let cfg = self.cfg.clone().expect("configure before start");
        let channel_count = cfg.channels.len().max(1) as u8;
        let stop_flag = self.stop_flag.clone();
        stop_flag.store(false, Ordering::SeqCst);

        tokio::spawn(async move {
            const CHUNK_SAMPLES: u32 = 1024;
            let mut sent: u64 = 0;
            while sent < cfg.samples {
                if stop_flag.load(Ordering::SeqCst) {
                    break;
                }
                let remaining = cfg.samples - sent;
                let n = CHUNK_SAMPLES.min(remaining as u32).max(1);
                let data = vec![0x42u8; (n as usize) * channel_count as usize];
                let chunk = SampleChunk {
                    encoding: Encoding::U8Interleaved,
                    channel_count,
                    sample_count: n,
                    t0_ns: sent * 1_000,
                    data: Bytes::from(data),
                };
                if sink.send(chunk).await.is_err() {
                    break;
                }
                sent += n as u64;
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        });

        Ok(CaptureId::new())
    }

    async fn stop(&mut self) -> CoreResult<()> {
        self.stop_flag.store(true, Ordering::SeqCst);
        Ok(())
    }
}
