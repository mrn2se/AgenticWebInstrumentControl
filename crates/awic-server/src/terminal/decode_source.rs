//! `decode` terminal source (SPECIFICATION.md §11.2): live protocol-decoder
//! output on a recording's channels — "UART bytes as text, I²C and SPI
//! transactions as one line each." Unlike `serial`/`log`, never shared or
//! joined: each `term.open` mints a fresh source id via `mint_fresh_id` and gets
//! its own decoder instance and its own live tail, because two opens can
//! name different decoder configurations even against the same recording.
//!
//! Hooks the recorder's append path the same way `rec.wait` does
//! (`crate::recorder::wait`): [`crate::recorder::decode_feed::DecodeListener`]
//! is registered on the target [`crate::recorder::RecordingEntry`] and fed
//! forward by `ingest.rs` after every append; this module only bridges that
//! feed into a [`SourceConnection`] the session manager can drive.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;

use awic_proto::{TerminalKind, TerminalSourceInfo};

use crate::recorder::decode_feed::{DecodeEvent, DecodeListener};
use crate::recorder::RecorderManager;

use super::session::{
    Connected, OpenSettings, SourceConnection, SourceEvent, TerminalError, TerminalSourceProvider,
};

/// The fixed picker entry's id (SPEC §11.2: "the Terminal tab's source
/// picker offers 'Decode…'"). A real session's id is minted per-open by
/// [`DecodeSourceProvider::mint_fresh_id`] and never reused, so `by_source`
/// (`session.rs`) never joins two different decode configurations.
pub const PICKER_SOURCE_ID: &str = "decode";

/// Depth of the channel between a [`DecodeListener`] (on the recorder's
/// append path) and its [`DecodeConnection`]: generous enough that one
/// append's worth of annotations doesn't itself trigger the per-connection
/// drop-and-count in `session.rs` (same reasoning as `log_source`'s
/// `BROADCAST_CAPACITY`).
const CHANNEL_CAPACITY: usize = 256;

pub struct DecodeSourceProvider {
    recorder: Arc<RecorderManager>,
    next_id: AtomicU32,
}

impl DecodeSourceProvider {
    pub fn new(recorder: Arc<RecorderManager>) -> Self {
        Self {
            recorder,
            next_id: AtomicU32::new(1),
        }
    }
}

#[async_trait]
impl TerminalSourceProvider for DecodeSourceProvider {
    fn kind(&self) -> TerminalKind {
        TerminalKind::Decode
    }

    fn list(&self) -> Vec<TerminalSourceInfo> {
        vec![TerminalSourceInfo {
            source: PICKER_SOURCE_ID.to_string(),
            kind: TerminalKind::Decode,
            label: "Decode a recording…".to_string(),
            device: None,
            available: true,
            unavailable_reason: None,
        }]
    }

    /// A minted `decode:<n>` id never appears in [`Self::list`] (SPEC:
    /// "never shared/joined like `serial`" — see the module doc), so
    /// `SessionManager::find` needs this override to resolve one.
    fn describe(&self, source_id: &str) -> Option<TerminalSourceInfo> {
        if source_id == PICKER_SOURCE_ID || source_id.starts_with("decode:") {
            Some(TerminalSourceInfo {
                source: source_id.to_string(),
                kind: TerminalKind::Decode,
                label: "Decode".to_string(),
                device: None,
                available: true,
                unavailable_reason: None,
            })
        } else {
            None
        }
    }

    fn persistent(&self) -> bool {
        false
    }

    /// Always mints a fresh `decode:<n>`, ignoring `base` (which is always
    /// the fixed picker id `"decode"`) — see the module doc's "never
    /// shared or joined."
    fn mint_fresh_id(&self, _base: &str) -> String {
        format!("decode:{}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    async fn connect(
        &self,
        _source_id: &str,
        settings: OpenSettings,
    ) -> Result<Connected, TerminalError> {
        let spec = settings
            .decode
            .ok_or_else(|| TerminalError::SettingsRequired(PICKER_SOURCE_ID.to_string()))?;
        let entry = self
            .recorder
            .get(spec.rec)
            .ok_or_else(|| TerminalError::Io(format!("no recording {}", spec.rec)))?;
        if !entry.is_recording() {
            return Err(TerminalError::Io(format!(
                "recording {} is not recording; the decode source only opens on a live recording",
                spec.rec
            )));
        }

        let mut instance = crate::decode::build_decoder(
            spec.decoder,
            &spec.channels,
            &spec.options,
            entry.rate_hz,
        )
        .map_err(|err| TerminalError::Io(err.to_string()))?;

        // Starts at the current tail rather than replaying retained history
        // (SPEC §11.2: "feed new runs ... into the session as it arrives") —
        // a live view like `log`/`serial`, not a backfill of everything the
        // recording has ever held. The bus may be mid-transaction right
        // there, so the decoder starts from the lines' actual levels.
        let (start_sample, levels) = {
            let rec = entry.read_rec();
            let head = rec.head_sample();
            let last = head
                .checked_sub(1)
                .and_then(|s| rec.runs(s, head).ok())
                .and_then(|runs| runs.first().map(|r| r.value));
            (head, last)
        };
        if let Some(levels) = levels {
            instance.seed_at(start_sample, levels);
        }
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let listener = Arc::new(DecodeListener::new(
            spec.decoder,
            instance,
            start_sample,
            tx,
        ));
        entry.register_decode_listener(listener);

        let label = format!("Decode {:?} on rec {}", spec.decoder, spec.rec);
        Ok(Connected {
            connection: Box::new(DecodeConnection { rx }),
            initial_backlog: Vec::new(),
            label,
        })
    }
}

struct DecodeConnection {
    rx: mpsc::Receiver<DecodeEvent>,
}

#[async_trait]
impl SourceConnection for DecodeConnection {
    async fn recv(&mut self) -> SourceEvent {
        match self.rx.recv().await {
            Some(DecodeEvent::Data(bytes)) => SourceEvent::Data(bytes),
            Some(DecodeEvent::Ended(reason)) => SourceEvent::Closed(Some(reason)),
            // The sending half was dropped without an explicit `Ended`
            // (shouldn't happen — `decode_feed::end_all` always sends one —
            // but a closed channel must still end the session, not hang it).
            None => SourceEvent::Closed(Some("recording ended".to_string())),
        }
    }

    async fn write(&mut self, _data: &[u8]) -> Result<(), TerminalError> {
        Err(TerminalError::Io(
            "the decode source is read-only".to_string(),
        ))
    }
}
