//! `log` terminal source (SPECIFICATION.md §12): a `tracing_subscriber`
//! [`Layer`] that turns every event at `INFO` and above — including the one
//! line every mutating request logs with its origin, SPEC §12's
//! "every mutation ... is logged ... and pushed to the UI" — into a colored
//! terminal line, kept in a ring of the last 1000 and broadcast live to any
//! open `log` session. Read-only: `SourceConnection::write` always refuses.
//!
//! The `INFO`-and-above cutoff (applied by the caller via
//! `Layer::with_filter`, see `main.rs`) plus each line's plain, uncolored
//! `[LEVEL]` prefix is what lets the terminal UI offer a client-side level
//! filter without a second wire op: the level is present in the text
//! itself, ANSI color codes aside.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use tokio::sync::broadcast;
use tracing::field::{Field, Visit};
use tracing::{Event as TracingEvent, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

use awic_proto::{TerminalKind, TerminalSourceInfo};

use super::session::{
    Connected, OpenSettings, SourceConnection, SourceEvent, TerminalError, TerminalSourceProvider,
};

/// The one `log` source's fixed id (SPEC §11.2.1).
pub const SOURCE_ID: &str = "log";

/// Lines kept for replay to a session that opens (or re-opens) `log` after
/// some history has already gone by.
const MAX_LINES: usize = 1000;

/// Broadcast channel depth: generous enough that a burst of log lines while
/// a session briefly can't keep up doesn't itself trigger the per-connection
/// drop-and-count in `session.rs` — that policy is meant to catch a slow
/// *client*, not a normal logging burst.
const BROADCAST_CAPACITY: usize = 4096;

fn ansi_for(level: &Level) -> &'static str {
    match *level {
        Level::ERROR => "\x1b[31m",
        Level::WARN => "\x1b[33m",
        Level::INFO => "\x1b[32m",
        Level::DEBUG => "\x1b[36m",
        Level::TRACE => "\x1b[90m",
    }
}
const RESET: &str = "\x1b[0m";

#[derive(Default)]
struct MessageVisitor {
    message: String,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.message, " {}={:?}", field.name(), value);
        }
    }
}

/// One tracing event, formatted as a line the terminal UI shows: a plain
/// (non-ANSI) `[LEVEL]` prefix a client-side filter can match on, then the
/// target and message in the level's color, `\r\n` so xterm's cursor
/// returns to column zero the way a real serial device's output would.
fn format_line(event: &TracingEvent<'_>) -> String {
    let level = *event.metadata().level();
    let mut visitor = MessageVisitor::default();
    event.record(&mut visitor);
    format!(
        "[{level:>5}] {color}{target}: {msg}{RESET}\r\n",
        color = ansi_for(&level),
        target = event.metadata().target(),
        msg = visitor.message,
    )
}

/// Installed into the global `tracing` subscriber (`main.rs`); the other
/// half of this pair, [`LogSource`], is what the terminal module opens
/// sessions against.
pub struct LogLayer {
    ring: Arc<StdMutex<VecDeque<String>>>,
    tx: broadcast::Sender<String>,
}

impl<S: Subscriber> Layer<S> for LogLayer {
    fn on_event(&self, event: &TracingEvent<'_>, _ctx: Context<'_, S>) {
        let line = format_line(event);
        {
            let mut ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
            ring.push_back(line.clone());
            if ring.len() > MAX_LINES {
                ring.pop_front();
            }
        }
        // No subscribers is not an error: nobody has the log terminal open.
        let _ = self.tx.send(line);
    }
}

/// The `log` [`TerminalSourceProvider`]. Persistent (SPEC §11.2.1's
/// "log sessions are read-only" and, implicitly, always there): opening it
/// joins one ever-running stream for the server's life rather than a
/// resource that gets closed when the last viewer leaves.
pub struct LogSource {
    ring: Arc<StdMutex<VecDeque<String>>>,
    tx: broadcast::Sender<String>,
}

/// Builds the paired [`LogLayer`] (install into the tracing subscriber) and
/// [`LogSource`] (register with the terminal [`super::SessionManager`]) —
/// two views of the same ring buffer and broadcast channel.
pub fn install() -> (LogLayer, LogSource) {
    let ring = Arc::new(StdMutex::new(VecDeque::with_capacity(MAX_LINES)));
    let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
    (
        LogLayer {
            ring: ring.clone(),
            tx: tx.clone(),
        },
        LogSource { ring, tx },
    )
}

struct LogConnection {
    rx: broadcast::Receiver<String>,
}

#[async_trait]
impl SourceConnection for LogConnection {
    async fn recv(&mut self) -> SourceEvent {
        loop {
            match self.rx.recv().await {
                Ok(line) => return SourceEvent::Data(line.into_bytes()),
                // A slow log reader lost some lines to the broadcast
                // channel's own bound; the session's own scrollback and
                // per-connection drop-and-count (session.rs) already give
                // clients a `[... dropped ...]` marker for *their* queue —
                // just resync and keep going rather than closing the session
                // over a burst of logging.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => {
                    return SourceEvent::Closed(Some("log stream ended".into()))
                }
            }
        }
    }

    async fn write(&mut self, _data: &[u8]) -> Result<(), TerminalError> {
        Err(TerminalError::Io("the log source is read-only".into()))
    }
}

#[async_trait]
impl TerminalSourceProvider for LogSource {
    fn kind(&self) -> TerminalKind {
        TerminalKind::Log
    }

    fn list(&self) -> Vec<TerminalSourceInfo> {
        vec![TerminalSourceInfo {
            source: SOURCE_ID.to_string(),
            kind: TerminalKind::Log,
            label: "Server log".to_string(),
            device: None,
            available: true,
            unavailable_reason: None,
        }]
    }

    fn persistent(&self) -> bool {
        true
    }

    async fn connect(
        &self,
        _source_id: &str,
        _settings: OpenSettings,
    ) -> Result<Connected, TerminalError> {
        let backlog: Vec<u8> = self
            .ring
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .flat_map(|line| line.bytes())
            .collect();
        Ok(Connected {
            connection: Box::new(LogConnection {
                rx: self.tx.subscribe(),
            }),
            initial_backlog: backlog,
            label: "Server log".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::Registry;

    #[tokio::test]
    async fn events_are_formatted_with_a_plain_level_prefix_and_replayed_on_connect() {
        let (layer, source) = install();
        let subscriber = Registry::default().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(
                origin = "ws 127.0.0.1:1234",
                "usb.power hub=3-3 port=4 on=false"
            );
        });

        let connected = source
            .connect(SOURCE_ID, OpenSettings::default())
            .await
            .unwrap();
        let text = String::from_utf8(connected.initial_backlog).unwrap();
        assert!(text.contains("[ INFO]"), "{text}");
        assert!(text.contains("usb.power hub=3-3 port=4 on=false"), "{text}");
    }

    #[test]
    fn list_reports_one_available_source() {
        let (_layer, source) = install();
        let sources = source.list();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].source, "log");
        assert!(sources[0].available);
    }
}
