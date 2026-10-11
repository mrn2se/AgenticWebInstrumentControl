//! Hot-plug detection (SPECIFICATION.md §4.3): one trait, two
//! implementations, selected by `[watcher] mode` in config
//! (`awic_core::config::WatcherMode`).
//!
//! Both implementations emit the same [`UsbEvent`]s over an unbounded
//! channel, so a caller (the `Registry`, wired up in `awic-server`) is
//! written once against [`DeviceWatcher`] and does not care which is
//! running underneath.

pub mod netlink;
pub mod polling;

use tokio::sync::mpsc;

use awic_core::{config::WatcherMode, DeviceId};

/// A hot-plug event, identical in shape regardless of which watcher produced
/// it. `DeviceId` is always built from the bus path, never the device
/// number (SPECIFICATION.md §2.3) — a re-enumeration that only changes
/// VID:PID (e.g. the FX2 firmware upload, §2.4) is *not* observed as a
/// remove-then-add by either watcher, because the bus path — the thing both
/// watchers actually key on — doesn't change across it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsbEvent {
    Added(DeviceId),
    Removed(DeviceId),
}

/// Detects USB device hot-plug and reports [`UsbEvent`]s.
///
/// `spawn` takes `self` by value and starts a background `tokio` task; it is
/// not `async fn` itself (spawning is synchronous), and the task runs until
/// it hits an unrecoverable error or the receiver is dropped.
pub trait DeviceWatcher {
    fn spawn(self) -> mpsc::UnboundedReceiver<UsbEvent>;
}

/// Builds and spawns the watcher selected by config (SPECIFICATION.md §4.3).
pub fn spawn_from_config(mode: WatcherMode) -> mpsc::UnboundedReceiver<UsbEvent> {
    match mode {
        WatcherMode::Netlink => netlink::NetlinkWatcher::new().spawn(),
        WatcherMode::Polling => polling::PollingWatcher::default().spawn(),
    }
}
