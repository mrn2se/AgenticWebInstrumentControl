//! `scpi` terminal source (SPECIFICATION.md §11.2, §9's "same parser as
//! port 5025"): a console on the same [`awic_scpi::Engine`] the TCP listener
//! drives, registered with [`crate::terminal::SessionManager`] exactly like
//! `log`/`serial` (`main.rs` wires all three in the same
//! `terminal_providers` vector).
//!
//! **Deliberate deviation from "own its own context" in the strictest
//! sense**: this source joins an already-open session exactly the way a
//! `serial` port does (SPEC §11.2.1 — "opening a port that is already open
//! joins the existing session"), rather than minting a private session per
//! browser tab. Two tabs open on `scpi` at once therefore share one
//! `Engine`/[`awic_scpi::Context`], the same as they'd share a real
//! instrument's single front panel. This is `SessionManager`'s
//! join-by-`source_id` design applied uniformly, not a special case — the
//! alternative (silently minting hidden per-connection source ids) would
//! make `term.sources`/`term.open`'s contract inconsistent between sources
//! for no benefit in the single-user, LAN-facing deployment SPEC §12
//! describes. What "each session is its own context" does guarantee: a
//! *fresh* session (nobody else has `scpi` open right now) always starts
//! from `*RST`-equivalent state, never inheriting a stale selection or
//! pending acquisition settings from a session that has since closed.

use std::collections::VecDeque;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;

use awic_proto::{TerminalKind, TerminalSourceInfo};
use awic_scpi::Engine;

use crate::state::AppState;
use crate::terminal::{
    Connected, OpenSettings, SourceConnection, SourceEvent, TerminalError, TerminalSourceProvider,
};

use super::ServerHost;

/// The one `scpi` source's fixed id (SPEC §11.2's table).
pub const SOURCE_ID: &str = "scpi";

/// Shown after every response and on connect — a human's cue that the
/// console is ready for the next line, the same role a shell prompt plays.
const PROMPT: &str = "SCPI> ";

/// Resolves the chicken-and-egg in `AppState::with_terminal_providers`: this
/// provider's `connect()` needs the *full* `AppState` (registry, recorder,
/// USB controller — everything `ServerHost` dispatches through), but
/// `AppState` only exists once every provider, this one included, has
/// already been constructed and handed to it. `main.rs` builds this cell,
/// passes a clone into [`ScpiTerminalSource::new`], builds `AppState`, then
/// fills the cell — by which point the server hasn't served a single
/// request yet, so `connect()` (only ever called for a real `term.open`)
/// always finds it set.
pub type LateAppState = Arc<OnceLock<AppState>>;

pub struct ScpiTerminalSource {
    state: LateAppState,
}

impl ScpiTerminalSource {
    pub fn new(state: LateAppState) -> Self {
        Self { state }
    }

    fn state(&self) -> AppState {
        self.state
            .get()
            .expect("AppState must be set (main.rs, right after constructing it) before any term.open can reach here")
            .clone()
    }
}

#[async_trait]
impl TerminalSourceProvider for ScpiTerminalSource {
    fn kind(&self) -> TerminalKind {
        TerminalKind::Scpi
    }

    /// Not persistent: once the last viewer leaves, the `Engine` and its
    /// `ServerHost` (and the connection id it registered with the recorder)
    /// are dropped and the next `term.open` starts fresh — the same
    /// lifecycle a `serial` port's session has (SPEC §11.2.1), and what
    /// backs the module doc's "a fresh session always starts clean" promise.
    fn persistent(&self) -> bool {
        false
    }

    /// Always one entry, always available (task brief: "the source is
    /// always available") — unlike `serial`, there is no hardware to be
    /// missing.
    fn list(&self) -> Vec<TerminalSourceInfo> {
        vec![TerminalSourceInfo {
            source: SOURCE_ID.to_string(),
            kind: TerminalKind::Scpi,
            label: "SCPI console".to_string(),
            device: None,
            available: true,
            unavailable_reason: None,
        }]
    }

    async fn connect(
        &self,
        _source_id: &str,
        _settings: OpenSettings,
    ) -> Result<Connected, TerminalError> {
        let host = Arc::new(ServerHost::new(self.state(), "scpi console".to_string()));
        let engine = Engine::new(host);
        Ok(Connected {
            connection: Box::new(ScpiConnection::new(engine)),
            // The prompt is scrollback, not a `SourceEvent` — a joiner sees
            // it exactly as if it had been there from the start, and a
            // brand-new session's very first thing shown is the prompt
            // itself rather than a blank screen waiting for output.
            initial_backlog: PROMPT.as_bytes().to_vec(),
            label: "SCPI console".to_string(),
        })
    }
}

/// One open `scpi` console: an [`Engine`] plus the byte-level plumbing
/// [`SourceConnection`] needs. `write`/`recv` are only ever called from the
/// single task `crate::terminal::session::run_session` drives (see that
/// module's doc comment), so `pending_output` needs no lock even though both
/// methods touch it.
struct ScpiConnection {
    engine: Engine,
    /// Bytes written by the client not yet forming a complete line (no
    /// trailing `\n` seen yet) — the client-side `LineEditor` (SPEC §11.2,
    /// web/src/modules/terminal) sends one full line per `term.write`, but
    /// nothing stops a script from writing partial lines or several at once.
    line_buf: Vec<u8>,
    /// Output queued for the next [`SourceConnection::recv`] call: the
    /// engine's response (if any), any errors it just queued (drained for
    /// display — see the module doc on `ServerHost`/`Engine::drain_errors`),
    /// and a fresh prompt.
    pending_output: VecDeque<u8>,
}

impl ScpiConnection {
    fn new(engine: Engine) -> Self {
        Self {
            engine,
            line_buf: Vec::new(),
            pending_output: VecDeque::new(),
        }
    }

    /// Runs one complete line (already stripped of its `\r`/`\n`) through
    /// the engine and queues its response, any newly queued errors, and a
    /// fresh prompt for `recv()` to deliver.
    async fn run_line(&mut self, line: &str) {
        if !line.is_empty() {
            if let Some(response) = self.engine.execute_line(line).await {
                self.pending_output.extend(response.into_bytes());
                self.pending_output.extend(b"\r\n");
            }
            // Terminal-only convenience (module doc): show errors as they
            // happen instead of making a human type `SYST:ERR?`. The TCP
            // listener never calls `drain_errors`, so `SYST:ERR?` there is
            // unaffected by this.
            for err in self.engine.drain_errors() {
                self.pending_output
                    .extend(format!("ERROR {}\r\n", err.to_wire()).into_bytes());
            }
        }
        self.pending_output.extend(PROMPT.as_bytes());
    }
}

#[async_trait]
impl SourceConnection for ScpiConnection {
    async fn recv(&mut self) -> SourceEvent {
        if self.pending_output.is_empty() {
            // Nothing to deliver until `write()` runs a line — `write()` and
            // `recv()` are raced by the same `select!` loop (module doc), so
            // this simply loses that race every time until there's
            // something queued; it never actually blocks the session.
            std::future::pending::<()>().await;
        }
        SourceEvent::Data(self.pending_output.drain(..).collect())
    }

    async fn write(&mut self, data: &[u8]) -> Result<(), TerminalError> {
        self.line_buf.extend_from_slice(data);
        while let Some(pos) = self.line_buf.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = self.line_buf.drain(..=pos).collect();
            let text = String::from_utf8_lossy(&raw);
            let line = text.trim_end_matches(['\r', '\n']).to_string();
            self.run_line(&line).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::PlaceholderBackend;
    use crate::usb::UnimplementedUsbController;
    use awic_core::Config;

    /// A ready-to-use provider, its backing `AppState` already set in the
    /// cell — standing in for `main.rs`'s real construction order.
    fn source() -> ScpiTerminalSource {
        let cell: LateAppState = Arc::new(OnceLock::new());
        cell.set(AppState::new(
            Config::default(),
            Arc::new(PlaceholderBackend),
            Arc::new(UnimplementedUsbController),
        ))
        .ok()
        .unwrap();
        ScpiTerminalSource::new(cell)
    }

    #[tokio::test]
    async fn list_reports_one_always_available_source() {
        let source = source();
        let sources = source.list();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].source, SOURCE_ID);
        assert!(sources[0].available);
    }

    #[tokio::test]
    async fn connect_seeds_the_prompt_as_scrollback() {
        let source = source();
        let connected = source
            .connect(SOURCE_ID, OpenSettings::default())
            .await
            .unwrap();
        assert_eq!(connected.initial_backlog, PROMPT.as_bytes());
    }

    #[tokio::test]
    async fn a_full_line_produces_a_response_then_a_fresh_prompt() {
        let source = source();
        let connected = source
            .connect(SOURCE_ID, OpenSettings::default())
            .await
            .unwrap();
        let mut conn = connected.connection;
        conn.write(b"*IDN?\n").await.unwrap();
        let SourceEvent::Data(bytes) = conn.recv().await else {
            panic!("expected data");
        };
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("AWIC,awic,0,"), "{text}");
        assert!(text.ends_with(PROMPT), "{text}");
    }

    #[tokio::test]
    async fn an_error_is_shown_inline_and_does_not_linger_in_the_queue() {
        let source = source();
        let connected = source
            .connect(SOURCE_ID, OpenSettings::default())
            .await
            .unwrap();
        let mut conn = connected.connection;
        conn.write(b"FOO:BAR?\n").await.unwrap();
        let SourceEvent::Data(bytes) = conn.recv().await else {
            panic!("expected data");
        };
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("ERROR -113"), "{text}");

        // A subsequent SYST:ERR? must find nothing left (already shown).
        conn.write(b"SYST:ERR?\n").await.unwrap();
        let SourceEvent::Data(bytes) = conn.recv().await else {
            panic!("expected data");
        };
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with(r#"0,"No error""#), "{text}");
    }
}
