//! Feeds the `decode` terminal source's live decoders on the recorder's
//! append path (SPECIFICATION.md §11.2), the same pattern `wait.rs` uses for
//! `rec.wait`: each open decode session registers a [`DecodeListener`]
//! holding its own decoder instance and resume point, driven forward by
//! [`feed_listeners`] right after every append.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex as StdMutex;

use tokio::sync::mpsc;

use awic_proto::DecoderId;

use super::RecordingEntry;
use crate::decode::render_terminal;

/// What a listener sends toward its terminal session
/// (`terminal::decode_source::DecodeConnection` turns this into
/// [`crate::terminal::SourceEvent`]).
pub enum DecodeEvent {
    Data(Vec<u8>),
    /// The recording stopped or errored (SPEC: "the session ends cleanly
    /// when the recording stops").
    Ended(String),
}

/// One open `decode` terminal session's state on the recording's append
/// path: its own decoder instance (so several sessions with different
/// configurations can watch the same recording independently, each at its
/// own pace) plus where it has decoded up to.
pub struct DecodeListener {
    decoder_id: DecoderId,
    decoder: StdMutex<Box<dyn awic_decode::Decoder>>,
    resume_sample: AtomicU64,
    tx: mpsc::Sender<DecodeEvent>,
}

impl DecodeListener {
    pub fn new(
        decoder_id: DecoderId,
        decoder: Box<dyn awic_decode::Decoder>,
        start_sample: u64,
        tx: mpsc::Sender<DecodeEvent>,
    ) -> Self {
        Self {
            decoder_id,
            decoder: StdMutex::new(decoder),
            resume_sample: AtomicU64::new(start_sample),
            tx,
        }
    }
}

/// Feeds every registered listener with whatever is newly appended, called
/// right after `wait::feed_waiters` on the same append (`ingest.rs`). A
/// listener whose channel has closed (its `decode` session's last subscriber
/// left, per `terminal::SessionManager`) is dropped here rather than kept
/// decoding into nowhere.
pub fn feed_listeners(entry: &RecordingEntry) {
    let rec = entry.read_rec();
    let head = rec.head_sample();
    let mut listeners = entry.decode_listeners.lock().unwrap();
    listeners.retain(|l| {
        if l.tx.is_closed() {
            return false;
        }
        let resume = l.resume_sample.load(Ordering::SeqCst);
        if resume >= head {
            return true;
        }
        let Ok(runs) = rec.runs(resume, head) else {
            return true; // shouldn't happen: resume/head stay in range.
        };
        if runs.is_empty() {
            l.resume_sample.store(head, Ordering::SeqCst);
            return true;
        }
        let known_until = runs.last().map(|r| r.start + r.len).unwrap_or(head);
        let input: Vec<awic_decode::Transition> = runs
            .iter()
            .map(|r| awic_decode::Transition {
                sample_index: r.start,
                value: r.value,
            })
            .collect();
        let anns = l
            .decoder
            .lock()
            .unwrap()
            .feed(&input, known_until.min(head));
        l.resume_sample.store(head, Ordering::SeqCst);
        if !anns.is_empty() {
            let bytes = render_terminal(l.decoder_id, &anns);
            if !bytes.is_empty() {
                let _ = l.tx.try_send(DecodeEvent::Data(bytes));
            }
        }
        true
    });
}

/// Resolves every still-open listener with `reason` (the recording just
/// stopped or errored — SPEC §11.2's "ends cleanly when the recording
/// stops"), mirroring `wait::end_all`.
pub fn end_all(entry: &RecordingEntry, reason: String) {
    let mut listeners = entry.decode_listeners.lock().unwrap();
    for l in listeners.drain(..) {
        let _ = l.tx.try_send(DecodeEvent::Ended(reason.clone()));
    }
}
