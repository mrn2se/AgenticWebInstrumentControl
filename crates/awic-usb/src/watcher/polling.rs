//! 1 Hz `/sys/bus/usb/devices` polling watcher (SPECIFICATION.md §4.3) — the
//! fallback for when the netlink watcher's network-namespace trap applies
//! (see `super::netlink`'s module docs) and `--net=host` isn't available.
//!
//! Detection latency rises to about one poll interval; everything else about
//! the resulting [`UsbEvent`]s is identical to the netlink watcher's.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use tokio::sync::mpsc;

use awic_core::DeviceId;

use super::{DeviceWatcher, UsbEvent};

const DEFAULT_ROOT: &str = "/sys/bus/usb/devices";
const DEFAULT_INTERVAL: Duration = Duration::from_secs(1);

/// One scan's worth of devices: identity plus VID:PID (for `Added` events —
/// `Removed` only needs identity, but we keep the map uniform).
type Scan = HashMap<DeviceId, (u16, u16)>;

/// Scans `root` for entries that are USB *devices* (hubs and leaves alike),
/// filtering out root hubs (`usbN`) and per-interface entries (`3-3:1.0`) by
/// requiring every character be a digit, `-`, or `.` — the same rule `nusb`
/// itself uses to distinguish device directories from everything else living
/// alongside them in `/sys/bus/usb/devices`.
fn scan(root: &Path) -> Scan {
    let mut found = Scan::new();
    let Ok(entries) = fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name
            .bytes()
            .all(|c| c.is_ascii_digit() || c == b'-' || c == b'.')
        {
            continue;
        }
        let Ok(id) = DeviceId::from_str(name) else {
            continue;
        };
        let path = entry.path();
        let vid = read_hex_u16(&path.join("idVendor")).unwrap_or(0);
        let pid = read_hex_u16(&path.join("idProduct")).unwrap_or(0);
        found.insert(id, (vid, pid));
    }
    found
}

fn read_hex_u16(path: &Path) -> Option<u16> {
    let s = fs::read_to_string(path).ok()?;
    u16::from_str_radix(s.trim(), 16).ok()
}

/// Diffs two scans into add/remove events.
///
/// A device present in both scans is never reported, even if its VID:PID
/// changed underneath it — that happens on every FX2 firmware upload
/// (SPECIFICATION.md §2.4) and is a `Registry`-level `flashing` -> `ready`
/// transition (§4.5), not a hot-unplug/replug as far as this watcher is
/// concerned; the bus path (this watcher's key, same as `DeviceId`'s
/// identity) didn't change.
fn diff(prev: &Scan, curr: &Scan) -> Vec<UsbEvent> {
    let mut events: Vec<UsbEvent> = curr
        .iter()
        .filter(|(id, _)| !prev.contains_key(*id))
        .map(|(id, &(vid, pid))| UsbEvent::Added(id.clone().with_vid_pid(vid, pid)))
        .collect();
    events.extend(
        prev.keys()
            .filter(|id| !curr.contains_key(*id))
            .cloned()
            .map(UsbEvent::Removed),
    );
    events
}

pub struct PollingWatcher {
    root: PathBuf,
    interval: Duration,
}

impl PollingWatcher {
    /// `root` is the directory to poll (normally
    /// `/sys/bus/usb/devices`) — overridable so tests can point it at a
    /// fixture directory with no real hardware involved.
    pub fn new(root: impl Into<PathBuf>, interval: Duration) -> Self {
        Self {
            root: root.into(),
            interval,
        }
    }
}

impl Default for PollingWatcher {
    fn default() -> Self {
        Self::new(DEFAULT_ROOT, DEFAULT_INTERVAL)
    }
}

impl DeviceWatcher for PollingWatcher {
    fn spawn(self) -> mpsc::UnboundedReceiver<UsbEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        // The baseline scan happens here, synchronously, before the
        // background task is even spawned — not as the first thing the
        // spawned task does. `tokio::spawn` only schedules the task; on a
        // current-thread runtime in particular it won't actually run until
        // the caller next awaits something, by which point the caller may
        // already have mutated the filesystem it expects to be diffed
        // against. Capturing `prev` here makes "state at the moment `spawn`
        // was called" the baseline, unambiguously.
        let mut prev = scan(&self.root);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(self.interval);
            loop {
                ticker.tick().await;
                let curr = scan(&self.root);
                for event in diff(&prev, &curr) {
                    if tx.send(event).is_err() {
                        return;
                    }
                }
                prev = curr;
            }
        });
        rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration as StdDuration;
    use tempfile::tempdir;
    use tokio::time::timeout;

    fn write_device(root: &Path, name: &str, vid: u16, pid: u16) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("idVendor"), format!("{vid:04x}\n")).unwrap();
        fs::write(dir.join("idProduct"), format!("{pid:04x}\n")).unwrap();
    }

    #[test]
    fn scan_ignores_root_hubs_and_interfaces() {
        let dir = tempdir().unwrap();
        write_device(dir.path(), "3-3", 0x2109, 0x2822);
        fs::create_dir_all(dir.path().join("usb3")).unwrap(); // root hub
        fs::create_dir_all(dir.path().join("3-3:1.0")).unwrap(); // interface

        let found = scan(dir.path());
        assert_eq!(found.len(), 1);
        assert!(found.contains_key(&DeviceId::new(3, vec![3], 0, 0)));
    }

    #[test]
    fn scan_of_missing_root_is_empty_not_an_error() {
        assert!(scan(Path::new("/does/not/exist")).is_empty());
    }

    #[test]
    fn diff_reports_additions_and_removals() {
        let mut prev = Scan::new();
        prev.insert(DeviceId::new(3, vec![3], 0x2109, 0x2822), (0x2109, 0x2822));
        prev.insert(
            DeviceId::new(3, vec![3, 3], 0x0925, 0x3881),
            (0x0925, 0x3881),
        );

        let mut curr = Scan::new();
        curr.insert(DeviceId::new(3, vec![3], 0x2109, 0x2822), (0x2109, 0x2822)); // unchanged
        curr.insert(
            DeviceId::new(3, vec![3, 4], 0x0bda, 0x5411),
            (0x0bda, 0x5411),
        ); // added

        let events = diff(&prev, &curr);
        assert_eq!(events.len(), 2);
        assert!(events.contains(&UsbEvent::Added(DeviceId::new(
            3,
            vec![3, 4],
            0x0bda,
            0x5411
        ))));
        assert!(events.contains(&UsbEvent::Removed(DeviceId::new(3, vec![3, 3], 0, 0))));
    }

    #[test]
    fn diff_ignores_a_vid_pid_change_at_the_same_bus_path() {
        // Same bus path, VID:PID changed underneath it (FX2 firmware
        // upload) — must NOT be reported as remove+add.
        let mut prev = Scan::new();
        prev.insert(
            DeviceId::new(3, vec![3, 4, 2], 0x04b4, 0x602a),
            (0x04b4, 0x602a),
        );

        let mut curr = Scan::new();
        curr.insert(
            DeviceId::new(3, vec![3, 4, 2], 0x1d50, 0x608e),
            (0x1d50, 0x608e),
        );

        assert!(diff(&prev, &curr).is_empty());
    }

    #[tokio::test]
    async fn watcher_reports_add_then_remove_against_a_fixture_directory() {
        let dir = tempdir().unwrap();
        write_device(dir.path(), "3-3", 0x2109, 0x2822);

        let mut rx = PollingWatcher::new(dir.path(), StdDuration::from_millis(20)).spawn();

        // Adding a new device between polls...
        write_device(dir.path(), "3-3.4", 0x0bda, 0x5411);
        let added = timeout(StdDuration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for add event")
            .expect("channel closed");
        assert_eq!(
            added,
            UsbEvent::Added(DeviceId::new(3, vec![3, 4], 0x0bda, 0x5411))
        );

        // ...then removing it...
        fs::remove_dir_all(dir.path().join("3-3.4")).unwrap();
        let removed = timeout(StdDuration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for remove event")
            .expect("channel closed");
        assert_eq!(
            removed,
            UsbEvent::Removed(DeviceId::new(3, vec![3, 4], 0, 0))
        );
    }
}
