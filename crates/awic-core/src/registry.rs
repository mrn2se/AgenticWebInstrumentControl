//! The device registry (SPECIFICATION.md §4.5).
//!
//! Tracks every device `awic-core` currently knows about, keyed by the
//! stable [`DeviceId`] (bus path — never the device number, never VID:PID
//! alone). Powering a port back on is not "the device returns": it
//! re-enumerates unprogrammed, gets flashed, and re-enumerates again
//! programmed. The `Registry` is what turns that three-step dance into the
//! single `flashing` -> `ready` transition the UI shows.

use std::collections::HashMap;
use std::sync::RwLock;

use awic_proto::{CapabilityInfo, DeviceInfo, Event, PROTOCOL_VERSION};

pub use awic_proto::DeviceState;

use crate::capability::Capability;
use crate::device_id::DeviceId;
use crate::event_bus::EventBus;

struct DeviceEntry {
    /// The device's *current* identity, VID:PID included.
    ///
    /// Deliberately duplicated here rather than read back off the map key.
    /// `DeviceId`'s `Eq`/`Hash` ignore VID:PID so that a re-flashed device
    /// stays the *same* device — but `HashMap::insert` keeps the ORIGINAL key
    /// when an equal one is already present, replacing only the value. Taking
    /// VID:PID from the key would therefore report the unprogrammed
    /// `04b4:602a` forever, even after the FX2 upload (§2.4) re-enumerated the
    /// device as `1d50:608e`.
    id: DeviceId,
    state: DeviceState,
    capabilities: Vec<Capability>,
    label: Option<String>,
}

/// The live device table plus the event bus it publishes changes to.
pub struct Registry {
    devices: RwLock<HashMap<DeviceId, DeviceEntry>>,
    events: EventBus,
}

impl Registry {
    pub fn new(events: EventBus) -> Self {
        Self {
            devices: RwLock::new(HashMap::new()),
            events,
        }
    }

    /// Records a freshly detected device, in [`DeviceState::Detected`], and
    /// publishes `device.added`. Calling this again for a `DeviceId` already
    /// present (e.g. VID:PID changed post-firmware-upload, but the bus path
    /// didn't) updates capabilities/label in place without re-publishing
    /// `device.added` — that transition is `set_state`'s job.
    pub fn detected(&self, id: DeviceId, capabilities: Vec<Capability>, label: Option<String>) {
        let mut devices = self.devices.write().expect("registry lock poisoned");
        let is_new = !devices.contains_key(&id);
        devices.insert(
            id.clone(),
            DeviceEntry {
                id: id.clone(),
                state: DeviceState::Detected,
                capabilities: capabilities.clone(),
                label: label.clone(),
            },
        );
        drop(devices);

        if is_new {
            self.events.publish(Event::DeviceAdded {
                v: PROTOCOL_VERSION,
                device: DeviceInfo {
                    id: id.to_string(),
                    vid: id.vid,
                    pid: id.pid,
                    label,
                    state: DeviceState::Detected,
                    capabilities: capabilities.iter().map(CapabilityInfo::from).collect(),
                },
            });
        }
    }

    /// Transitions a known device to `state`, publishing `device.state`.
    /// `msg` carries error detail for [`DeviceState::Error`]. No-op (with a
    /// `tracing::warn`) if `id` is not known — a state update racing a
    /// removal is expected, not a bug.
    pub fn set_state(&self, id: &DeviceId, state: DeviceState, msg: Option<String>) {
        let mut devices = self.devices.write().expect("registry lock poisoned");
        match devices.get_mut(id) {
            Some(entry) => entry.state = state,
            None => {
                tracing::warn!(%id, ?state, "set_state on unknown device");
                return;
            }
        }
        drop(devices);

        self.events.publish(Event::DeviceState {
            v: PROTOCOL_VERSION,
            device: id.to_string(),
            state,
            msg,
        });
    }

    /// Removes a device and publishes `device.removed`. No-op if unknown.
    pub fn remove(&self, id: &DeviceId) {
        let removed = self
            .devices
            .write()
            .expect("registry lock poisoned")
            .remove(id)
            .is_some();
        if removed {
            self.events.publish(Event::DeviceRemoved {
                v: PROTOCOL_VERSION,
                device: id.to_string(),
            });
        }
    }

    pub fn state_of(&self, id: &DeviceId) -> Option<DeviceState> {
        self.devices
            .read()
            .expect("registry lock poisoned")
            .get(id)
            .map(|e| e.state)
    }

    pub fn contains(&self, id: &DeviceId) -> bool {
        self.devices
            .read()
            .expect("registry lock poisoned")
            .contains_key(id)
    }

    pub fn len(&self) -> usize {
        self.devices.read().expect("registry lock poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Snapshots every known device as the wire-facing [`DeviceInfo`], for
    /// e.g. a `device.list` response.
    pub fn list(&self) -> Vec<DeviceInfo> {
        self.devices
            .read()
            .expect("registry lock poisoned")
            .iter()
            .map(|(id, entry)| DeviceInfo {
                id: id.to_string(),
                vid: entry.id.vid,
                pid: entry.id.pid,
                label: entry.label.clone(),
                state: entry.state,
                capabilities: entry
                    .capabilities
                    .iter()
                    .map(CapabilityInfo::from)
                    .collect(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev() -> DeviceId {
        DeviceId::new(3, vec![3, 4, 2], 0x04b4, 0x602a)
    }

    #[tokio::test]
    async fn detecting_a_new_device_publishes_device_added() {
        let registry = Registry::new(EventBus::default());
        let mut rx = registry.events.subscribe();

        registry.detected(dev(), vec![], Some("FX2 oscilloscope".into()));

        let event = rx.recv().await.unwrap();
        match event {
            Event::DeviceAdded { device, .. } => {
                assert_eq!(device.id, "3-3.4.2");
                assert_eq!(device.label.as_deref(), Some("FX2 oscilloscope"));
                assert_eq!(device.state, DeviceState::Detected);
            }
            other => panic!("expected DeviceAdded, got {other:?}"),
        }
        assert_eq!(registry.state_of(&dev()), Some(DeviceState::Detected));
    }

    #[tokio::test]
    async fn re_enumeration_at_same_bus_path_does_not_re_add() {
        let registry = Registry::new(EventBus::default());
        registry.detected(dev(), vec![], None);

        let mut rx = registry.events.subscribe();
        // Same bus path, new VID:PID post-firmware-upload (§2.4) — DeviceId
        // equality ignores VID:PID, so this is the *same* device.
        let reflashed = dev().with_vid_pid(0x1d50, 0x608e);
        registry.detected(reflashed, vec![], None);

        assert_eq!(registry.len(), 1, "must not create a second entry");
        // No DeviceAdded fires the second time.
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn re_enumeration_updates_vid_pid_in_place() {
        let registry = Registry::new(EventBus::default());
        registry.detected(dev(), vec![], None);

        // The FX2 upload re-enumerates the oscilloscope 04b4:602a -> 1d50:608e at
        // the same bus path. It is the same device, but its VID:PID must now
        // read as the programmed pair or driver matching sees a stale ID.
        registry.detected(dev().with_vid_pid(0x1d50, 0x608e), vec![], None);

        let listed = registry.list();
        assert_eq!(listed.len(), 1, "must not create a second entry");
        assert_eq!(listed[0].vid, 0x1d50, "post-flash VID must be visible");
        assert_eq!(listed[0].pid, 0x608e, "post-flash PID must be visible");
    }

    #[tokio::test]
    async fn state_transition_publishes_device_state_event() {
        let registry = Registry::new(EventBus::default());
        registry.detected(dev(), vec![], None);
        let mut rx = registry.events.subscribe();

        registry.set_state(&dev(), DeviceState::Flashing, None);

        let event = rx.recv().await.unwrap();
        assert_eq!(
            event,
            Event::DeviceState {
                v: PROTOCOL_VERSION,
                device: "3-3.4.2".into(),
                state: DeviceState::Flashing,
                msg: None,
            }
        );
        assert_eq!(registry.state_of(&dev()), Some(DeviceState::Flashing));
    }

    #[tokio::test]
    async fn removing_a_device_publishes_device_removed() {
        let registry = Registry::new(EventBus::default());
        registry.detected(dev(), vec![], None);
        let mut rx = registry.events.subscribe();

        registry.remove(&dev());

        let event = rx.recv().await.unwrap();
        assert_eq!(
            event,
            Event::DeviceRemoved {
                v: PROTOCOL_VERSION,
                device: "3-3.4.2".into(),
            }
        );
        assert!(!registry.contains(&dev()));
    }

    #[tokio::test]
    async fn removing_an_unknown_device_is_a_silent_no_op() {
        let registry = Registry::new(EventBus::default());
        let mut rx = registry.events.subscribe();
        registry.remove(&dev());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn list_reflects_current_devices() {
        let registry = Registry::new(EventBus::default());
        assert!(registry.list().is_empty());
        registry.detected(dev(), vec![], None);
        let listed = registry.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "3-3.4.2");
    }
}
