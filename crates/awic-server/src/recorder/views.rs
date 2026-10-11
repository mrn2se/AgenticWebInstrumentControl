//! `rec.view`/`rec.view.update`/`rec.view.close` (SPECIFICATION.md §3.4):
//! per-connection view tasks that turn a recording into 0xA6 view frames.
//!
//! A **follow** view runs a background task ticking at up to `fps`,
//! recomputing the trailing `follow_s`-second window each time; it is
//! **conflated**, never queued — [`run_follow`]'s `in_flight` flag mirrors
//! `crate::backpressure::StreamBackpressure`'s release-on-send-complete
//! pattern (see `ws.rs`'s writer), just for "at most one frame in flight"
//! instead of a byte counter. A **fixed** view has no task: it sends once on
//! open and once per `rec.view.update`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use awic_proto::view_frame::{encode_view_frame, ViewFrameFlags, ViewFrameHeader, ViewFrameKind};

use super::{RecOpError, RecorderManager, RecordingEntry, RecordingKindLocal};
use crate::ws::WsOut;

/// A view's live parameters (SPEC §3.4). `follow_s.is_some()` means a follow
/// (live-tail) view; `None` means fixed, read from `t0_s`/`t1_s`.
#[derive(Debug, Clone, Copy)]
pub struct ViewParams {
    pub follow_s: Option<f64>,
    pub t0_s: Option<f64>,
    pub t1_s: Option<f64>,
    pub columns: u32,
    pub fps: u32,
}

struct FollowHandle {
    /// Needed to look the recording back up when [`update`] switches this
    /// view to fixed (SPEC §5.5's pause) — `run_follow` itself is handed the
    /// `Arc<RecordingEntry>` directly and never needs this, but the handle
    /// bookkeeping here does.
    rec: u16,
    /// A clone of the same sender `run_follow`'s task holds, kept here too
    /// so [`update`] can push one immediate fixed-view frame right after
    /// aborting the task, without waiting for a tick that will never come.
    out_tx: mpsc::Sender<WsOut>,
    params_tx: watch::Sender<ViewParams>,
    task: JoinHandle<()>,
}

struct FixedHandle {
    rec: u16,
    seq: u32,
    params: ViewParams,
    out_tx: mpsc::Sender<WsOut>,
}

/// Per-connection view registry (SPEC §3.4: "views belong to the connection
/// that opened them and die with it"). Dropping this aborts every follow
/// view's task; a fixed view has no task to abort, just bookkeeping.
pub struct ConnViews {
    next_id: AtomicU16,
    follow: StdMutex<HashMap<u16, FollowHandle>>,
    fixed: StdMutex<HashMap<u16, FixedHandle>>,
}

impl Default for ConnViews {
    fn default() -> Self {
        Self {
            next_id: AtomicU16::new(1),
            follow: StdMutex::new(HashMap::new()),
            fixed: StdMutex::new(HashMap::new()),
        }
    }
}

impl Drop for ConnViews {
    fn drop(&mut self) {
        for (_, h) in self.follow.lock().unwrap().drain() {
            h.task.abort();
        }
    }
}

impl ConnViews {
    fn next_view_id(&self) -> u16 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }
}

fn fps_period(fps: u32) -> std::time::Duration {
    std::time::Duration::from_secs_f64(1.0 / fps.clamp(1, 60) as f64)
}

/// `rec.view`: opens a view, sending the first frame (fixed) or starting the
/// ticking task (follow).
pub async fn open(
    manager: &RecorderManager,
    conn_id: u64,
    out_tx: mpsc::Sender<WsOut>,
    rec: u16,
    params: ViewParams,
) -> Result<u16, RecOpError> {
    let entry = manager.get(rec).ok_or_else(|| RecOpError::not_found(rec))?;
    let conn = manager
        .conn_views(conn_id)
        .ok_or_else(|| RecOpError::new("internal_error", "connection is gone"))?;
    let view_id = conn.next_view_id();

    if params.follow_s.is_some() {
        let (tx, rx) = watch::channel(params);
        let task = tokio::spawn(run_follow(entry, view_id, rx, out_tx.clone()));
        conn.follow.lock().unwrap().insert(
            view_id,
            FollowHandle {
                rec,
                out_tx,
                params_tx: tx,
                task,
            },
        );
    } else {
        let bytes = build_frame(&entry, view_id, 0, false, params);
        let in_flight = Arc::new(AtomicBool::new(true));
        let _ = out_tx.send(WsOut::ViewFrame { bytes, in_flight }).await;
        conn.fixed.lock().unwrap().insert(
            view_id,
            FixedHandle {
                rec,
                seq: 1,
                params,
                out_tx,
            },
        );
    }
    Ok(view_id)
}

/// A `rec.view.update` patch: omitted (`None`) fields keep their current
/// value (SPEC §3.4). Bundled into one struct rather than five parameters
/// so [`update`] stays under clippy's argument-count lint.
#[derive(Debug, Clone, Copy, Default)]
pub struct ViewUpdate {
    pub follow_s: Option<f64>,
    pub t0_s: Option<f64>,
    pub t1_s: Option<f64>,
    pub columns: Option<u32>,
    pub fps: Option<u32>,
}

/// `rec.view.update`: omitted fields keep their current value (SPEC §3.4).
///
/// A patch can also switch the view's *kind*, not just tweak parameters of
/// whichever kind it already is — SPEC §5.5's pause/resume round-trip
/// depends on this: "zooming or panning pauses following ... the view
/// switches to a fixed range served from history (`rec.view.update`), and a
/// *Follow* button resumes [it]." A patch naming `t0_s`/`t1_s` (and not
/// `follow_s`) means "become fixed at this range"; one naming `follow_s`
/// means "become (or stay) a follow view". Each direction stops the old
/// mode's machinery (aborts the ticking task, or simply forgets the fixed
/// bookkeeping) and starts the new one's, exactly as `open` would for a
/// fresh view — the view id and its connection ownership don't change.
pub async fn update(
    manager: &RecorderManager,
    conn_id: u64,
    view: u16,
    patch: ViewUpdate,
) -> Result<(), RecOpError> {
    let conn = manager
        .conn_views(conn_id)
        .ok_or_else(|| RecOpError::new("internal_error", "connection is gone"))?;
    let switch_to_fixed =
        patch.follow_s.is_none() && (patch.t0_s.is_some() || patch.t1_s.is_some());
    let switch_to_follow = patch.follow_s.is_some();

    // --- Currently a follow view ---
    let existing_follow = conn.follow.lock().unwrap().remove(&view);
    if let Some(handle) = existing_follow {
        if switch_to_fixed {
            handle.task.abort();
            let prev = *handle.params_tx.borrow();
            let params = ViewParams {
                follow_s: None,
                t0_s: patch.t0_s.or(prev.t0_s),
                t1_s: patch.t1_s.or(prev.t1_s),
                columns: patch.columns.unwrap_or(prev.columns),
                fps: prev.fps,
            };
            let entry = manager
                .get(handle.rec)
                .ok_or_else(|| RecOpError::not_found(handle.rec))?;
            let bytes = build_frame(&entry, view, 0, false, params);
            let in_flight = Arc::new(AtomicBool::new(true));
            // `handle` was already removed from `conn.follow` above, so a
            // send failure (connection gone) just means this view's frame
            // is dropped along with everything else on this connection —
            // nothing left to roll back.
            let _ = handle
                .out_tx
                .send(WsOut::ViewFrame { bytes, in_flight })
                .await;
            conn.fixed.lock().unwrap().insert(
                view,
                FixedHandle {
                    rec: handle.rec,
                    seq: 1,
                    params,
                    out_tx: handle.out_tx,
                },
            );
            return Ok(());
        }
        // In-kind update: adjust follow_s/columns/fps, put the handle back.
        let mut params = *handle.params_tx.borrow();
        if let Some(f) = patch.follow_s {
            params.follow_s = Some(f);
        }
        if let Some(c) = patch.columns {
            params.columns = c;
        }
        if let Some(f) = patch.fps {
            params.fps = f.clamp(1, 60);
        }
        let _ = handle.params_tx.send(params);
        conn.follow.lock().unwrap().insert(view, handle);
        return Ok(());
    }

    // --- Currently a fixed view ---
    let existing_fixed = conn.fixed.lock().unwrap().remove(&view);
    if let Some(handle) = existing_fixed {
        if switch_to_follow {
            let mut params = handle.params;
            params.follow_s = patch.follow_s;
            if let Some(c) = patch.columns {
                params.columns = c;
            }
            if let Some(f) = patch.fps {
                params.fps = f.clamp(1, 60);
            }
            let entry = manager
                .get(handle.rec)
                .ok_or_else(|| RecOpError::not_found(handle.rec))?;
            let (tx, rx) = watch::channel(params);
            let task = tokio::spawn(run_follow(entry, view, rx, handle.out_tx.clone()));
            conn.follow.lock().unwrap().insert(
                view,
                FollowHandle {
                    rec: handle.rec,
                    out_tx: handle.out_tx,
                    params_tx: tx,
                    task,
                },
            );
            return Ok(());
        }
        // In-kind update: adjust t0_s/t1_s/columns, send the refreshed frame.
        let mut params = handle.params;
        if let Some(t0) = patch.t0_s {
            params.t0_s = Some(t0);
        }
        if let Some(t1) = patch.t1_s {
            params.t1_s = Some(t1);
        }
        if let Some(c) = patch.columns {
            params.columns = c;
        }
        let seq = handle.seq;
        let entry = manager
            .get(handle.rec)
            .ok_or_else(|| RecOpError::not_found(handle.rec))?;
        let bytes = build_frame(&entry, view, seq, false, params);
        let in_flight = Arc::new(AtomicBool::new(true));
        let _ = handle
            .out_tx
            .send(WsOut::ViewFrame { bytes, in_flight })
            .await;
        conn.fixed.lock().unwrap().insert(
            view,
            FixedHandle {
                rec: handle.rec,
                seq: seq.wrapping_add(1),
                params,
                out_tx: handle.out_tx,
            },
        );
        return Ok(());
    }

    Err(RecOpError::new("view_not_found", format!("no view {view}")))
}

/// `rec.view.close`.
pub fn close(manager: &RecorderManager, conn_id: u64, view: u16) -> Result<(), RecOpError> {
    let conn = manager
        .conn_views(conn_id)
        .ok_or_else(|| RecOpError::new("internal_error", "connection is gone"))?;
    if let Some(handle) = conn.follow.lock().unwrap().remove(&view) {
        handle.task.abort();
        return Ok(());
    }
    if conn.fixed.lock().unwrap().remove(&view).is_some() {
        return Ok(());
    }
    Err(RecOpError::new("view_not_found", format!("no view {view}")))
}

async fn run_follow(
    entry: Arc<RecordingEntry>,
    view_id: u16,
    mut params_rx: watch::Receiver<ViewParams>,
    out_tx: mpsc::Sender<WsOut>,
) {
    let mut sequence: u32 = 0;
    let in_flight = Arc::new(AtomicBool::new(false));
    let mut params = *params_rx.borrow();
    let mut interval = tokio::time::interval(fps_period(params.fps));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            changed = params_rx.changed() => {
                if changed.is_err() {
                    break; // Sender dropped: `rec.view.close` or the connection closed.
                }
                params = *params_rx.borrow();
                interval = tokio::time::interval(fps_period(params.fps));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            }
            _ = interval.tick() => {
                // Conflated: skip this tick if the previous frame hasn't
                // been flushed to the socket yet (SPEC §3.4).
                if in_flight.swap(true, Ordering::AcqRel) {
                    continue;
                }
                let bytes = build_frame(&entry, view_id, sequence, true, params);
                sequence = sequence.wrapping_add(1);
                let msg = WsOut::ViewFrame { bytes, in_flight: in_flight.clone() };
                match out_tx.try_send(msg) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        // The connection's shared outbound queue is
                        // saturated (the client isn't draining at all, even
                        // at this view's own reduced rate): drop this
                        // tick's frame rather than block or grow memory,
                        // the same policy `forward_stream` uses for sample
                        // frames (SPEC §3.2/§3.4). Nothing was queued, so
                        // no frame is actually in flight — clear the flag.
                        in_flight.store(false, Ordering::Release);
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => break, // Connection gone.
                }
            }
        }
    }
}

/// Computes the view's current window and encodes the resulting frame
/// (SPEC §3.4). Clamps the window to the retained span and sets the
/// `truncated` flag rather than erroring, unlike `Recording::view`'s own
/// `Evicted` error — a live view must degrade gracefully, not fail, when
/// eviction eats into the range it was asked to show.
fn build_frame(
    entry: &RecordingEntry,
    view_id: u16,
    sequence: u32,
    follow: bool,
    params: ViewParams,
) -> Vec<u8> {
    let (first, head) = {
        let g = entry.read_rec();
        (g.first_sample(), g.head_sample())
    };
    let rate = entry.rate_hz.max(1) as f64;

    let (want_start, want_end) = if let Some(follow_s) = params.follow_s {
        let span = (follow_s * rate).round().max(0.0) as u64;
        (head.saturating_sub(span), head)
    } else {
        let t0 = params.t0_s.unwrap_or(0.0);
        let t1 = params.t1_s.unwrap_or(head as f64 / rate);
        (
            (t0 * rate).round().max(0.0) as u64,
            (t1 * rate).round().max(0.0) as u64,
        )
    };
    let truncated = want_start < first;
    let start = want_start.max(first);
    let end = want_end.min(head);

    let view_data = if start >= end {
        empty_view_data(entry, start)
    } else {
        let g = entry.read_rec();
        g.view(start, end, params.columns.max(1))
            .unwrap_or_else(|_| empty_view_data(entry, start))
    };

    let header = ViewFrameHeader {
        view_id,
        sequence,
        kind: map_kind(view_data.kind),
        channel_count: view_data.channel_count,
        sample_bits: view_data.sample_bits,
        flags: ViewFrameFlags { follow, truncated },
        item_count: view_data.item_count.min(u32::MAX as u64) as u32,
        first_sample: view_data.first_sample,
        samples_per_item: view_data.samples_per_item,
    };
    encode_view_frame(&header, &view_data.payload)
}

fn empty_view_data(entry: &RecordingEntry, first_sample: u64) -> awic_record::ViewData {
    let (kind, sample_bits, channel_count) = match entry.kind {
        RecordingKindLocal::Digital => (awic_record::ViewKind::DigitalRuns, 8u8, 8u8),
        RecordingKindLocal::Analog {
            channel_count,
            sample_bits,
        } => (awic_record::ViewKind::AnalogRaw, sample_bits, channel_count),
    };
    awic_record::ViewData {
        kind,
        first_sample,
        samples_per_item: 1,
        item_count: 0,
        channel_count,
        sample_bits,
        payload: Vec::new(),
    }
}

fn map_kind(k: awic_record::ViewKind) -> ViewFrameKind {
    match k {
        awic_record::ViewKind::DigitalRuns => ViewFrameKind::DigitalRuns,
        awic_record::ViewKind::DigitalSummary => ViewFrameKind::DigitalSummary,
        awic_record::ViewKind::AnalogRaw => ViewFrameKind::AnalogRaw,
        awic_record::ViewKind::AnalogSummary => ViewFrameKind::AnalogSummary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use awic_core::{
        AcqConfig, Capability, CaptureId, DeviceId, EffectiveAcq, EventBus, Instrument,
        Result as CoreResult, SampleSink,
    };
    use std::time::Duration;
    use tokio::sync::Mutex as AsyncMutex;

    /// Never actually called by these tests — `RecordingEntry::instrument`
    /// only needs to exist to satisfy the type, since nothing here calls
    /// `rec.stop`/`rec.start`.
    struct NoopInstrument(DeviceId);

    #[async_trait]
    impl Instrument for NoopInstrument {
        fn id(&self) -> &DeviceId {
            &self.0
        }
        fn capabilities(&self) -> &[Capability] {
            &[]
        }
        async fn prepare(&mut self) -> CoreResult<()> {
            Ok(())
        }
        async fn configure(&mut self, cfg: AcqConfig) -> CoreResult<EffectiveAcq> {
            Ok(EffectiveAcq::digital(cfg))
        }
        async fn start(&mut self, _sink: SampleSink) -> CoreResult<CaptureId> {
            Ok(CaptureId::new())
        }
        async fn stop(&mut self) -> CoreResult<()> {
            Ok(())
        }
    }

    /// Inserts a bare digital recording directly into `manager`, bypassing
    /// `recorder::start` (which needs a running instrument/device) — these
    /// tests only exercise the view machinery against a `RecordingEntry`
    /// that already has some data.
    fn insert_digital_entry(manager: &RecorderManager, samples: &[u8]) -> Arc<RecordingEntry> {
        let device_id: DeviceId = "1-1".parse().unwrap();
        let instrument: crate::state::SharedInstrument =
            Arc::new(AsyncMutex::new(Box::new(NoopInstrument(device_id.clone()))));
        let entry = manager.insert(
            "1-1".to_string(),
            device_id,
            1_000_000,
            RecordingKindLocal::Digital,
            vec![super::super::RecChannel {
                channel: 0,
                name: "D0".to_string(),
                calibration: awic_core::ChannelCalibration::digital(0),
            }],
            instrument,
        );
        entry.append(samples).unwrap();
        entry
    }

    fn manager() -> RecorderManager {
        RecorderManager::new(
            256,
            EventBus::default(),
            Arc::new(awic_core::CaptureStore::with_defaults()),
        )
    }

    /// SPEC §3.4: "frames are conflated, never queued — if the client has
    /// not drained the previous frame, the next tick is skipped." Proven
    /// directly against the view machinery (no real socket, no timing
    /// flakiness): an `out_tx` of capacity 1 that this test never drains at
    /// all, ticking many times over, must never accumulate more than its
    /// bounded capacity worth of frames — the in-flight flag plus
    /// `try_send`-and-drop keep the connection's queue from growing no
    /// matter how many ticks fire while nobody reads.
    #[tokio::test]
    async fn a_client_that_never_drains_gets_conflation_not_a_growing_queue() {
        let manager = manager();
        let entry = insert_digital_entry(&manager, &[0u8; 1000]);
        let conn_id = manager.new_connection();
        let (out_tx, mut out_rx) = mpsc::channel::<WsOut>(1);

        let params = ViewParams {
            follow_s: Some(0.0001),
            t0_s: None,
            t1_s: None,
            columns: 100,
            // Fast enough that, over the sleep below, dozens of ticks would
            // fire if nothing conflated them.
            fps: 200,
        };
        let view_id = open(&manager, conn_id, out_tx, entry.id, params)
            .await
            .expect("open");

        // Real time, not virtual: `tokio::time::interval` under a paused
        // clock needs the test to drive advancement itself, which adds
        // complexity for no benefit here — 150ms real time at 200 fps is
        // "dozens of ticks" either way, and the assertion only needs an
        // upper bound, not an exact count.
        tokio::time::sleep(Duration::from_millis(150)).await;

        let mut frames = 0usize;
        while let Ok(msg) = out_rx.try_recv() {
            if let WsOut::ViewFrame { in_flight, .. } = msg {
                in_flight.store(false, Ordering::Release);
            }
            frames += 1;
        }
        assert!(
            frames <= 1,
            "expected at most the channel's bounded capacity (1) worth of \
             frames from a never-drained view, got {frames}"
        );

        close(&manager, conn_id, view_id).unwrap();
        manager.close_connection(conn_id);
    }

    /// The same view, actually drained on every tick, must receive more
    /// than one frame — otherwise the test above would be vacuous (passing
    /// merely because nothing was ever sent at all).
    #[tokio::test]
    async fn a_client_that_drains_promptly_receives_multiple_frames() {
        let manager = manager();
        let entry = insert_digital_entry(&manager, &[0u8; 1000]);
        let conn_id = manager.new_connection();
        let (out_tx, mut out_rx) = mpsc::channel::<WsOut>(8);

        let params = ViewParams {
            follow_s: Some(0.0001),
            t0_s: None,
            t1_s: None,
            columns: 100,
            fps: 100,
        };
        let view_id = open(&manager, conn_id, out_tx, entry.id, params)
            .await
            .expect("open");

        let mut frames = 0usize;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        while tokio::time::Instant::now() < deadline {
            if let Ok(Some(WsOut::ViewFrame { in_flight, .. })) =
                tokio::time::timeout(Duration::from_millis(20), out_rx.recv()).await
            {
                in_flight.store(false, Ordering::Release);
                frames += 1;
            }
        }
        assert!(
            frames >= 2,
            "expected multiple frames when the client actually drains, got {frames}"
        );

        close(&manager, conn_id, view_id).unwrap();
        manager.close_connection(conn_id);
    }

    /// SPEC §5.5: "zooming or panning pauses following ... the view
    /// switches to a fixed range served from history (`rec.view.update`),
    /// and a *Follow* button resumes." Regression test for the bug this
    /// fixes: `update()` used to only ever adjust parameters of a view's
    /// *existing* kind, silently ignoring `t0_s`/`t1_s` sent to a follow
    /// view (and `follow_s` sent to a fixed one) — so a paused Live view
    /// kept following the tail forever, and Follow could never resume a
    /// paused one either.
    #[tokio::test]
    async fn view_update_switches_follow_to_fixed_and_back() {
        let manager = manager();
        let entry = insert_digital_entry(&manager, &[0xffu8; 4000]);
        let conn_id = manager.new_connection();
        let (out_tx, mut out_rx) = mpsc::channel::<WsOut>(8);

        let params = ViewParams {
            follow_s: Some(0.001),
            t0_s: None,
            t1_s: None,
            columns: 100,
            fps: 100,
        };
        let view_id = open(&manager, conn_id, out_tx, entry.id, params)
            .await
            .expect("open");

        // Drain whatever `open()`'s first tick(s) already produced.
        while let Ok(WsOut::ViewFrame { in_flight, .. }) = out_rx.try_recv() {
            in_flight.store(false, Ordering::Release);
        }

        // Pause: switch to a fixed range. This must actually take effect.
        update(
            &manager,
            conn_id,
            view_id,
            ViewUpdate {
                t0_s: Some(0.0),
                t1_s: Some(0.0005),
                ..Default::default()
            },
        )
        .await
        .expect("update to fixed");

        let msg = tokio::time::timeout(Duration::from_millis(200), out_rx.recv())
            .await
            .expect("a frame should be pushed immediately on switching to fixed")
            .expect("channel open");
        let WsOut::ViewFrame { bytes, in_flight } = msg else {
            panic!("expected a view frame")
        };
        in_flight.store(false, Ordering::Release);
        // Byte 11 is the flags byte (SPEC §3.4); bit 0 is "follow".
        assert_eq!(
            bytes[11] & 0b01,
            0,
            "expected the follow flag cleared after switching to fixed"
        );

        // A fixed view has no ticking task — confirm silence over a window
        // that would have seen several follow ticks at 100 fps.
        let silence = tokio::time::timeout(Duration::from_millis(100), out_rx.recv()).await;
        assert!(
            silence.is_err(),
            "a fixed view must not keep ticking like a follow view"
        );

        // Resume: switch back to follow.
        update(
            &manager,
            conn_id,
            view_id,
            ViewUpdate {
                follow_s: Some(0.001),
                ..Default::default()
            },
        )
        .await
        .expect("update back to follow");

        let mut saw_follow_frame = false;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
        while tokio::time::Instant::now() < deadline {
            if let Ok(Some(WsOut::ViewFrame { bytes, in_flight })) =
                tokio::time::timeout(Duration::from_millis(20), out_rx.recv()).await
            {
                in_flight.store(false, Ordering::Release);
                assert_eq!(bytes[11] & 0b01, 1, "expected the follow flag set again");
                saw_follow_frame = true;
                break;
            }
        }
        assert!(
            saw_follow_frame,
            "expected the view to resume ticking after switching back to follow"
        );

        close(&manager, conn_id, view_id).unwrap();
        manager.close_connection(conn_id);
    }
}
