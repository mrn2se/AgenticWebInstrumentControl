//! Ingest: drains one recording's sample channel and appends into its
//! `awic_record::Recording` (SPECIFICATION.md §2.8).
//!
//! Runs on `spawn_blocking`, draining with `blocking_recv` (DEVELOPMENT.md risk R11):
//! `Recording::append_packed`/`append_analog` and the summary tree update
//! are synchronous CPU work with no `.await` inside them, so running them on
//! the async executor would block every other task sharing that worker
//! thread for however long a big chunk's RLE/delta encoding takes. A
//! blocking OS thread is the correct home for that work, exactly as
//! DEVELOPMENT.md §1 forbids blocking calls *in async fns* — this fn is
//! deliberately not one.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use awic_core::SampleChunk;

use super::{wait, RecordingEntry, RecordingKindLocal};

/// How often one recording resumes after a stall before it gives up and ends
/// with the error, as it always did before B-25's resume.
const MAX_RESUMES: u32 = 3;
/// Filler is appended in chunks of this many samples, so a long gap never
/// needs one buffer the size of the gap.
const FILL_CHUNK: usize = 1 << 20;

/// Spawns the ingest task for a just-started recording. Ends when the
/// producer's `SampleSink` closes (the device stopped, was told to via
/// `rec.stop`, or the driver errored) — see `RecordingEntry::stop_requested`
/// for how the ending is classified — unless the device stalled while still
/// attached, in which case a digital recording resumes (B-25).
pub fn spawn(
    entry: Arc<RecordingEntry>,
    rx: mpsc::Receiver<SampleChunk>,
    state: crate::state::AppState,
) -> JoinHandle<()> {
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || run(entry, rx, state, handle))
}

fn run(
    entry: Arc<RecordingEntry>,
    mut rx: mpsc::Receiver<SampleChunk>,
    state: crate::state::AppState,
    handle: tokio::runtime::Handle,
) {
    let manager = state.recorder().clone();
    let registry = state.registry().clone();
    // When the last chunk arrived, and — after a resume — the time the gap
    // began, so its length can be filled in once data flows again.
    let mut last_chunk_at = Instant::now();
    let mut gap_since: Option<Instant> = None;
    loop {
        while let Some(chunk) = rx.blocking_recv() {
            if let Some(since) = gap_since.take() {
                fill_gap(&entry, since, chunk.data.len());
            }
            last_chunk_at = Instant::now();
            if let Err(err) = entry.append(&chunk.data) {
                tracing::error!(rec = entry.id, %err, "recorder append failed");
                entry.mark_error(err.to_string());
                wait::end_all(&entry, err.to_string());
                super::decode_feed::end_all(&entry, err.to_string());
                manager.publish_state(&entry);
                return;
            }
            wait::feed_waiters(&entry);
            super::decode_feed::feed_listeners(&entry);
            manager.evict_to_budget();
        }

        // The sink closed without a rec.stop, the device is still there, and
        // the driver resets a device that streamed and then stalled at its
        // next start: start again rather than end. A recording used to end
        // here, losing everything after — on the bench, every reset of an
        // ESP32 behind the analyzer's hub (bench run 2026-10-09).
        if !entry.stop_was_requested()
            && registry.contains(&entry.device_id)
            && matches!(entry.kind, RecordingKindLocal::Digital)
            && entry.resumes() < MAX_RESUMES
        {
            let samples = entry.read_rec().head_sample();
            tracing::warn!(
                rec = entry.id,
                device = %entry.device,
                samples,
                attempt = entry.resumes() + 1,
                "recording stalled; restarting the device and resuming"
            );
            match handle.block_on(restart(&state, &entry)) {
                Ok(new_rx) => {
                    entry.count_resume();
                    rx = new_rx;
                    gap_since = Some(last_chunk_at);
                    continue;
                }
                Err(err) => {
                    tracing::warn!(rec = entry.id, %err, "could not resume the recording");
                }
            }
        }
        break;
    }

    // The sink closed. `stop_requested` distinguishes `rec.stop` (or
    // `rec.delete`'s stop step) from the device disappearing or the driver
    // erroring on its own — both close the same channel from the
    // instrument's side, so this flag is the only signal available here,
    // exactly as `crate::dispatch::run_acquisition_stream` does for finite
    // acquisitions.
    if entry.stop_was_requested() {
        entry.mark_stopped();
        wait::end_all(&entry, "recording stopped".to_string());
        super::decode_feed::end_all(&entry, "recording stopped".to_string());
    } else {
        // "disconnected, stalled, or a driver error" was one sentence for three
        // different things, and a recording displaced by another `rec.start`
        // got it too (B-59/B-60) - every guess in it wrong. The registry
        // answers the one question that distinguishes the cases: is the device
        // still on the bus?
        let samples = entry.read_rec().head_sample();
        let attached = registry.contains(&entry.device_id);
        let msg = end_reason(&entry.device, samples, attached);
        // Nothing used to be logged here at all: a recording could die for any
        // reason and leave no trace in the server's own log (B-61).
        tracing::warn!(
            rec = entry.id,
            device = %entry.device,
            rate_hz = entry.rate_hz,
            samples,
            attached,
            "recording ended without rec.stop: {msg}"
        );
        entry.mark_error(msg.clone());
        wait::end_all(&entry, msg.clone());
        super::decode_feed::end_all(&entry, msg);
    }
    manager.publish_state(&entry);
}

/// Starts the recording's instrument again on a fresh sink. The driver keeps
/// its configuration and resets a device that stalled (firmware re-upload)
/// inside `start`, which `start_announced` reports as `flashing` (B-87).
async fn restart(
    state: &crate::state::AppState,
    entry: &RecordingEntry,
) -> Result<mpsc::Receiver<SampleChunk>, String> {
    let (sink, rx) = awic_core::SampleSink::channel(64);
    let mut guard = entry.instrument.lock().await;
    if entry.stop_was_requested() {
        return Err("a stop was requested meanwhile".into());
    }
    state
        .start_announced(&entry.device_id, &mut guard, sink)
        .await
        .map_err(|e| e.to_string())?;
    Ok(rx)
}

/// Fills the time the device was being reset with its last level, so every
/// sample after the gap is still at its true time, and records the span.
/// The length comes from the wall clock: from the last chunk before the
/// stall to the first one after, less that first chunk's own samples.
fn fill_gap(entry: &RecordingEntry, since: Instant, first_chunk_samples: usize) {
    let elapsed = since.elapsed().as_secs_f64();
    let missing = (elapsed * entry.rate_hz as f64) as u64;
    let missing = missing.saturating_sub(first_chunk_samples as u64);
    let start = entry.read_rec().head_sample();
    if missing == 0 {
        entry.add_gap(start, start);
        return;
    }
    let last = {
        let rec = entry.read_rec();
        rec.runs(start.saturating_sub(1), start)
            .ok()
            .and_then(|runs| runs.last().map(|r| r.value))
            .unwrap_or(0)
    };
    let chunk = vec![last; FILL_CHUNK];
    let mut left = missing;
    while left > 0 {
        let n = left.min(FILL_CHUNK as u64) as usize;
        if let Err(err) = entry.append(&chunk[..n]) {
            tracing::error!(rec = entry.id, %err, "could not fill the gap");
            break;
        }
        left -= n as u64;
    }
    entry.add_gap(start, start + missing);
    tracing::info!(
        rec = entry.id,
        start,
        samples = missing,
        seconds = missing as f64 / entry.rate_hz as f64,
        "recording resumed; gap filled with the last level"
    );
}

/// Why a recording ended when nothing asked it to.
///
/// "disconnected, stalled, or a driver error" was one sentence for three
/// different causes, and before B-59 a recording displaced by a second
/// `rec.start` was handed it too - every guess in it wrong (B-60). The registry
/// answers the one question that separates the cases, so the reason states it
/// instead of listing possibilities, and says how much was recorded before the
/// stream ended.
fn end_reason(device: &str, samples: u64, attached: bool) -> String {
    if attached {
        format!(
            "the device stopped streaming after {samples} samples with no rec.stop, while still \
             attached: a driver stall or error. The server log for {device:?} at this instant says \
             which - a stall at a rate the USB path cannot sustain is the usual cause"
        )
    } else {
        format!(
            "the device left the bus after {samples} samples - {device:?} was unplugged or its \
             port power was cut; it is no longer in the registry"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::end_reason;

    /// B-60: the bench saw this message on a recording that had been displaced
    /// by another `rec.start`, where all three of its guesses were wrong.
    #[test]
    fn the_reason_states_what_the_registry_knows_rather_than_guessing() {
        let attached = end_reason("3-3.4.1.1", 1_344_000, true);
        assert!(attached.contains("still attached"), "{attached}");
        assert!(attached.contains("1344000 samples"), "{attached}");
        assert!(attached.contains("3-3.4.1.1"), "{attached}");
        assert!(
            attached.contains("server log"),
            "must say where to look: {attached}"
        );

        let gone = end_reason("3-3.4.2", 0, false);
        assert!(gone.contains("left the bus"), "{gone}");
        assert!(gone.contains("0 samples"), "{gone}");
        assert!(gone.contains("port power"), "{gone}");

        // Neither may fall back to listing every possible cause.
        for msg in [attached, gone] {
            assert!(
                !msg.contains("disconnected, stalled, or a driver error"),
                "one sentence for three causes is back: {msg}"
            );
        }
    }
}

/// B-25: a recording whose device stalls while still attached resumes, with
/// the missing time filled with the last level and listed as a gap.
#[cfg(test)]
mod resume_tests {
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use awic_core::{
        AcqConfig, Backend, Capability, CaptureId, Config, DeviceId, Instrument,
        Result as CoreResult, SampleChunk, SampleSink,
    };
    use awic_proto::frame::Encoding;
    use awic_proto::RecordingState;
    use bytes::Bytes;

    use crate::state::AppState;

    /// Streams 1000 samples of 0x01 then 1000 of 0x03 and closes its sink —
    /// a stall — on its first start; on later starts it waits 50 ms (the
    /// device being reset) and streams 1000 samples of 0x05, then keeps the
    /// sink open.
    struct StallingInstrument {
        id: DeviceId,
        starts: u32,
        held: Option<SampleSink>,
    }

    fn chunk(value: u8, n: usize) -> SampleChunk {
        SampleChunk {
            encoding: Encoding::PackedDigital,
            channel_count: 2,
            sample_count: n as u32,
            t0_ns: 0,
            data: Bytes::from(vec![value; n]),
        }
    }

    #[async_trait]
    impl Instrument for StallingInstrument {
        fn id(&self) -> &DeviceId {
            &self.id
        }
        fn capabilities(&self) -> &[Capability] {
            &[]
        }
        async fn prepare(&mut self) -> CoreResult<()> {
            Ok(())
        }
        async fn configure(&mut self, cfg: AcqConfig) -> CoreResult<awic_core::EffectiveAcq> {
            Ok(awic_core::EffectiveAcq::digital(cfg))
        }
        async fn start(&mut self, sink: SampleSink) -> CoreResult<CaptureId> {
            self.starts += 1;
            if self.starts == 1 {
                tokio::spawn(async move {
                    let _ = sink.send(chunk(0x01, 1000)).await;
                    let _ = sink.send(chunk(0x03, 1000)).await;
                    // Dropped: the stream stalls.
                });
            } else {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let _ = sink.send(chunk(0x05, 1000)).await;
                self.held = Some(sink);
            }
            Ok(CaptureId::new())
        }
        async fn stop(&mut self) -> CoreResult<()> {
            self.held = None;
            Ok(())
        }
    }

    struct StallingBackend;

    #[async_trait]
    impl Backend for StallingBackend {
        fn name(&self) -> &str {
            "stalling"
        }
        async fn discover(&self) -> CoreResult<Vec<DeviceId>> {
            Ok(vec![])
        }
        async fn open(&self, id: &DeviceId) -> CoreResult<Box<dyn Instrument>> {
            Ok(Box::new(StallingInstrument {
                id: id.clone(),
                starts: 0,
                held: None,
            }))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stalled_recording_resumes_with_the_gap_filled_and_listed() {
        let state = AppState::new(
            Config::default(),
            Arc::new(StallingBackend),
            Arc::new(crate::usb::UnimplementedUsbController),
        );
        let id: DeviceId = "3-3.4.1.1".parse().unwrap();
        state.registry().detected(id.clone(), vec![], None);
        state
            .registry()
            .set_state(&id, awic_proto::DeviceState::Ready, None);

        let cfg = AcqConfig {
            rate_hz: 1_000_000,
            channels: vec![0, 1],
            samples: 0,
            trigger: None,
            ranges_v: vec![],
            continuous: true,
        };
        let (rec, _) = crate::recorder::start(&state, id.to_string(), cfg)
            .await
            .expect("rec.start");

        let entry = state.recorder().get(rec).unwrap();
        let mut summary = None;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let s = state
                .recorder()
                .list()
                .recordings
                .into_iter()
                .find(|r| r.rec == rec)
                .unwrap();
            if !s.gaps.is_empty() && s.head_sample > 3000 {
                summary = Some(s);
                break;
            }
        }
        let s = summary.expect("the recording resumed and listed a gap");
        assert_eq!(s.state, RecordingState::Recording, "{s:?}");
        assert_eq!(s.gaps.len(), 1);
        let gap = s.gaps[0];
        assert_eq!(
            gap.start_sample, 2000,
            "the gap starts where the data stopped"
        );
        // ~50 ms at 1 MHz, measured on the wall clock.
        let len = gap.end_sample - gap.start_sample;
        assert!((20_000..500_000).contains(&len), "gap of {len} samples");
        assert_eq!(s.head_sample, gap.end_sample + 1000);

        // The gap holds the last level; the data after it is intact.
        {
            let rec_guard = entry.read_rec();
            let runs = rec_guard.runs(500, s.head_sample).unwrap();
            let values: Vec<(u8, u64)> = runs.iter().map(|r| (r.value, r.len)).collect();
            assert_eq!(values, vec![(0x01, 500), (0x03, 1000 + len), (0x05, 1000)]);
        }

        // A result over the gap says so.
        let note = entry.range_note(0, s.head_sample).expect("a note");
        assert!(note.contains("no data in"), "{note}");
        assert!(
            entry.range_note(0, 1000).is_none(),
            "no note before the gap"
        );

        // A rec.stop still ends it as stopped, not as a stall to resume from.
        crate::recorder::stop(&state, rec).await.expect("rec.stop");
        for _ in 0..100 {
            if entry.state() == RecordingState::Stopped {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(entry.state(), RecordingState::Stopped);
    }
}
