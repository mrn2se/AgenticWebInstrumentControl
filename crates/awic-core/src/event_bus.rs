//! `awic-core`'s event bus, over `tokio::sync::broadcast`.
//!
//! `Registry` publishes [`awic_proto::Event`]s directly — the same type that
//! crosses the `/ws` wire (SPECIFICATION.md §3.1) — so a transport only has
//! to subscribe and serialize; there is no separate internal event type to
//! keep in sync with the wire one.

use tokio::sync::broadcast;

use awic_proto::Event;

/// Cheap to clone: internally just an `Arc`-backed broadcast sender, exactly
/// like `tokio::sync::broadcast::Sender` itself.
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

/// Matches `CaptureStore`'s default capacity order of magnitude; deliberately
/// small since events are meant to be consumed live (every mutation is also
/// logged, per SPECIFICATION.md §12), not replayed from deep history.
const DEFAULT_CAPACITY: usize = 256;

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self { tx }
    }

    /// A new receiver only sees events published after it subscribes —
    /// standard `broadcast` semantics.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    /// Publishes an event to every current subscriber. A publish with no
    /// subscribers is not an error: the UI may simply not be connected.
    pub fn publish(&self, event: Event) {
        let _ = self.tx.send(event);
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awic_proto::PROTOCOL_VERSION;

    #[tokio::test]
    async fn subscriber_receives_published_event() {
        let bus = EventBus::default();
        let mut rx = bus.subscribe();

        bus.publish(Event::DeviceRemoved {
            v: PROTOCOL_VERSION,
            device: "3-3.4.2".into(),
        });

        let event = rx.recv().await.unwrap();
        assert_eq!(
            event,
            Event::DeviceRemoved {
                v: PROTOCOL_VERSION,
                device: "3-3.4.2".into(),
            }
        );
    }

    #[tokio::test]
    async fn publish_with_no_subscribers_does_not_panic() {
        let bus = EventBus::default();
        bus.publish(Event::DeviceRemoved {
            v: PROTOCOL_VERSION,
            device: "3-3.4.2".into(),
        });
    }

    #[tokio::test]
    async fn multiple_subscribers_all_receive_the_event() {
        let bus = EventBus::default();
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();

        bus.publish(Event::DeviceRemoved {
            v: PROTOCOL_VERSION,
            device: "1-1".into(),
        });

        assert!(a.recv().await.is_ok());
        assert!(b.recv().await.is_ok());
    }
}
