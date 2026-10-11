//! The real [`UsbController`], backed by `awic-usb` (SPECIFICATION.md §4).
//!
//! Builds the wire-facing tree with **ports as first-class nodes**: every
//! hub lists every downstream port, attached or empty. That is not cosmetic.
//! Power belongs to the port; if it lived on device nodes, switching a port
//! off would unpower its device, the device would drop off the bus, its node
//! would vanish, and the only control able to switch the port back on would
//! vanish with it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::RwLock;

use async_trait::async_trait;
use awic_core::config::WatcherMode;
use awic_core::{port_protection, DeviceId, EventBus, PortOverrides, PORT_OVERRIDES_FILE};
use awic_proto::{
    Event, UsbChange, UsbLabelResult, UsbPort, UsbPowerResult, UsbProtectResult, UsbTreeNode,
    UsbTreeResult, PROTOCOL_VERSION,
};
use awic_usb::{TopologyNode, UsbError, UsbEvent};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;

use crate::error::{core_error_code, UsbOpError};
use crate::usb::UsbController;

/// Starts the configured hot-plug watcher (SPEC §4.3) and publishes every bus
/// change as a `usb.changed` event. Without this the watcher exists but never
/// runs: the tree would only refresh after the user's own toggle, and a device
/// that re-enumerates a second after power-on would never appear.
pub fn spawn_topology_watcher(mode: WatcherMode, events: EventBus) -> JoinHandle<()> {
    tracing::info!(?mode, "starting usb hot-plug watcher");
    tokio::spawn(forward_usb_events(
        awic_usb::spawn_from_config(mode),
        events,
    ))
}

/// Forwards watcher output onto the event bus. Split from
/// [`spawn_topology_watcher`] so it is testable with a plain channel.
pub async fn forward_usb_events(mut rx: UnboundedReceiver<UsbEvent>, events: EventBus) {
    while let Some(event) = rx.recv().await {
        let (device, change) = match event {
            UsbEvent::Added(id) => (id, UsbChange::Added),
            UsbEvent::Removed(id) => (id, UsbChange::Removed),
        };
        tracing::debug!(%device, ?change, "usb topology changed");
        events.publish(Event::UsbChanged {
            v: PROTOCOL_VERSION,
            device: device.to_string(),
            change,
        });
    }
    // A watcher that stops silently leaves the UI showing a frozen tree.
    tracing::warn!("usb hot-plug watcher ended; the tree will no longer refresh on its own");
}

/// VID:PIDs of instruments AWIC drives. Mirrors the native drivers; the
/// The oscilloscope appears twice because it re-enumerates across its firmware upload.
/// `0925:3881` is the fx2lafw logic analyzer *and* the oscilloscope in logic-switch position
/// (SPEC §5.1) — an instrument either way.
const KNOWN_INSTRUMENTS: &[(u16, u16)] = &[(0x04b4, 0x602a), (0x1d50, 0x608e), (0x0925, 0x3881)];

pub struct RealUsbController {
    /// Session file holding the user's lock/unlock choices (SPEC §4.4).
    state_file: PathBuf,
    overrides: RwLock<PortOverrides>,
}

impl RealUsbController {
    /// Discovers switchable ports from the bus — there is nothing to
    /// configure — and loads the user's earlier lock/unlock choices from
    /// `state_dir`.
    pub fn new(state_dir: &Path) -> Self {
        let state_file = state_dir.join(PORT_OVERRIDES_FILE);
        let overrides = PortOverrides::load(&state_file);
        Self {
            state_file,
            overrides: RwLock::new(overrides),
        }
    }

    fn overrides(&self) -> PortOverrides {
        self.overrides
            .read()
            .expect("port overrides lock poisoned")
            .clone()
    }
}

#[async_trait]
impl UsbController for RealUsbController {
    async fn tree(&self) -> Result<UsbTreeResult, UsbOpError> {
        let buses = awic_usb::enumerate().await.map_err(usb_op_error)?;

        // Only EMPTY switchable ports need a status read: a device that
        // enumerates on a port already proves that port is powered. The empty
        // ones are exactly the interesting case — a port switched off, whose
        // device has vanished, must still report `power: false` so its toggle
        // survives.
        let mut empty = Vec::new();
        for bus in &buses {
            for root in &bus.roots {
                collect_empty_switchable(root, &mut empty);
            }
        }
        let mut empty_power = HashMap::new();
        for (hub, port) in empty {
            let powered = awic_usb::power::get_port_power(&hub, port)
                .await
                .ok()
                .map(|status| status.powered);
            empty_power.insert((hub, port), powered);
        }

        let overrides = self.overrides();
        let roots = buses
            .iter()
            .flat_map(|bus| bus.roots.iter())
            .map(|node| to_wire(node, &overrides, &empty_power))
            .collect();
        Ok(UsbTreeResult { roots })
    }

    async fn power(&self, hub: &str, port: u8, on: bool) -> Result<UsbPowerResult, UsbOpError> {
        // A snapshot: the lock is not held across the hardware I/O.
        let overrides = self.overrides();
        let status = awic_usb::power::set_port_power(&overrides, hub, port, on)
            .await
            .map_err(usb_op_error)?;
        Ok(UsbPowerResult {
            hub: hub.to_string(),
            port,
            on: status.powered,
        })
    }

    async fn protect(
        &self,
        hub: &str,
        port: u8,
        protected: Option<bool>,
    ) -> Result<UsbProtectResult, UsbOpError> {
        let hub_id = DeviceId::from_str(hub).map_err(|e| UsbOpError {
            code: "invalid_hub_id".into(),
            msg: format!("{hub:?} is not a valid hub identifier: {e}"),
        })?;
        if port == 0 {
            return Err(UsbOpError {
                code: "invalid_port".into(),
                msg: "ports are numbered from 1".into(),
            });
        }
        let snapshot = {
            let mut overrides = self
                .overrides
                .write()
                .expect("port overrides lock poisoned");
            overrides.set(hub, port, protected);
            overrides.clone()
        };
        // Persisted before answering: a choice the user saw accepted must
        // survive a restart.
        snapshot.save(&self.state_file).map_err(|e| UsbOpError {
            code: "state_write_failed".into(),
            msg: format!(
                "could not save port state to {}: {e}",
                self.state_file.display()
            ),
        })?;
        // A lock or an unlock decides on its own (`port_protection`), so only
        // "back to automatic" needs to know what is attached. Enumerating
        // regardless answered an error for a choice that had just been saved.
        let derived = match protected {
            Some(_) => None,
            None => awic_usb::power::attached_protection(&hub_id, port)
                .await
                .map_err(usb_op_error)?,
        };
        let reason = port_protection(derived.as_deref(), protected);
        Ok(UsbProtectResult {
            hub: hub.to_string(),
            port,
            protected: reason.is_some(),
            reason,
            user_choice: protected,
        })
    }

    async fn label(
        &self,
        hub: &str,
        port: u8,
        label: Option<String>,
        dut: Option<bool>,
    ) -> Result<UsbLabelResult, UsbOpError> {
        DeviceId::from_str(hub).map_err(|e| UsbOpError {
            code: "invalid_hub_id".into(),
            msg: format!("{hub:?} is not a valid hub identifier: {e}"),
        })?;
        if port == 0 {
            return Err(UsbOpError {
                code: "invalid_port".into(),
                msg: "ports are numbered from 1".into(),
            });
        }
        let (entry, snapshot) = {
            let mut overrides = self
                .overrides
                .write()
                .expect("port overrides lock poisoned");
            let entry = overrides.set_label(hub, port, label, dut);
            (entry, overrides.clone())
        };
        snapshot.save(&self.state_file).map_err(|e| UsbOpError {
            code: "state_write_failed".into(),
            msg: format!(
                "could not save port state to {}: {e}",
                self.state_file.display()
            ),
        })?;
        Ok(UsbLabelResult {
            hub: hub.to_string(),
            port,
            label: entry.label,
            dut: entry.dut,
        })
    }
}

/// The port number `child` occupies on its parent hub: the last element of
/// its bus path.
fn port_of(child: &TopologyNode) -> Option<u8> {
    child.device_id.port_path.last().copied()
}

/// Empty ports of power-switchable hubs, for a status read — whether or not
/// they are protected: a locked port's power state is still worth showing.
fn collect_empty_switchable(node: &TopologyNode, out: &mut Vec<(String, u8)>) {
    if node.is_hub && node.ppps == Some(true) {
        let hub = node.device_id.to_string();
        for port in 1..=node.num_ports.unwrap_or(0) {
            if !node.children.iter().any(|c| port_of(c) == Some(port)) {
                out.push((hub.clone(), port));
            }
        }
    }
    for child in &node.children {
        collect_empty_switchable(child, out);
    }
}

fn to_wire(
    node: &TopologyNode,
    overrides: &PortOverrides,
    empty_power: &HashMap<(String, u8), Option<bool>>,
) -> UsbTreeNode {
    let id = &node.device_id;
    let hub = id.to_string();
    let ports = if node.is_hub {
        (1..=node.num_ports.unwrap_or(0))
            .map(|number| {
                let child = node.children.iter().find(|c| port_of(c) == Some(number));
                let power = match child {
                    // Enumerating proves power even where the status read failed.
                    Some(c) => c.upstream_port_power.or(Some(true)),
                    None => empty_power.get(&(hub.clone(), number)).copied().flatten(),
                };
                let user_choice = overrides.get(&hub, number);
                let derived_protection = child.and_then(|c| c.derived_protection.clone());
                let protection = port_protection(derived_protection.as_deref(), user_choice);
                let hardware_limit = hardware_limit(node);
                UsbPort {
                    number,
                    power,
                    power_switchable: hardware_limit.is_none(),
                    protected: protection.is_some(),
                    derived_protection,
                    user_choice,
                    label: overrides.label(&hub, number).and_then(|l| l.label.clone()),
                    dut: overrides.label(&hub, number).is_some_and(|l| l.dut),
                    switchable: hardware_limit.is_none() && protection.is_none(),
                    non_switchable_reason: hardware_limit.or_else(|| protection.clone()),
                    protection,
                    device: child.map(|c| Box::new(to_wire(c, overrides, empty_power))),
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    UsbTreeNode {
        bus: id.bus,
        port_path: id.port_path.clone(),
        vid: id.vid,
        pid: id.pid,
        product: node.product.clone(),
        speed: node.speed.clone(),
        is_hub: node.is_hub,
        ppps: node.ppps,
        is_instrument: is_instrument(id),
        ports,
    }
}

/// Why this hub cannot switch its ports at all — a hardware fact, distinct
/// from protection — or `None` if it can.
fn hardware_limit(hub_node: &TopologyNode) -> Option<String> {
    match hub_node.ppps {
        None => Some(
            "hub descriptor unreadable — install tools/udev/99-awic.rules or run rootful".into(),
        ),
        Some(false) => Some("this hub does not support per-port power switching".into()),
        Some(true) => None,
    }
}

fn is_instrument(id: &DeviceId) -> bool {
    KNOWN_INSTRUMENTS.contains(&(id.vid, id.pid))
}

/// Maps `awic-usb` failures onto stable wire codes. Guardrail refusals reuse
/// core's own codes, so a refused port reads identically whichever layer
/// caught it.
fn usb_op_error(err: UsbError) -> UsbOpError {
    let code = match &err {
        UsbError::Guardrail(core) => core_error_code(core),
        UsbError::NotPowerSwitchable { .. } => "port_not_switchable",
        UsbError::InvalidPort { .. } => "invalid_port",
        UsbError::HubNotFound(_) => "hub_not_found",
        UsbError::InvalidHubId(..) => "invalid_hub_id",
        UsbError::Open { .. } => "usb_access_denied",
        _ => "usb_error",
    };
    UsbOpError {
        code: code.to_string(),
        msg: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(path: &[u8], vid: u16, pid: u16) -> DeviceId {
        DeviceId::new(3, path.to_vec(), vid, pid)
    }

    fn hub(path: &[u8], vid: u16, pid: u16, children: Vec<TopologyNode>) -> TopologyNode {
        let id = dev(path, vid, pid);
        TopologyNode {
            derived_protection: Some(format!(
                "feeds hub {id}: switching it cuts every device behind it"
            )),
            device_id: id,
            product: None,
            manufacturer: None,
            speed: None,
            is_hub: true,
            ppps: Some(true),
            num_ports: Some(4),
            upstream_port_switchable: false,
            upstream_port_power: None,
            children,
        }
    }

    fn leaf(path: &[u8], vid: u16, pid: u16) -> TopologyNode {
        TopologyNode {
            device_id: dev(path, vid, pid),
            product: None,
            manufacturer: None,
            speed: None,
            is_hub: false,
            ppps: None,
            num_ports: None,
            upstream_port_switchable: true,
            upstream_port_power: Some(true),
            derived_protection: None,
            children: vec![],
        }
    }

    /// The real bench topology: nothing configured, protection derived.
    fn bench() -> TopologyNode {
        hub(
            &[3],
            0x2109,
            0x2822,
            vec![
                leaf(&[3, 3], 0x0925, 0x3881),
                hub(
                    &[3, 4],
                    0x0bda,
                    0x5411,
                    vec![leaf(&[3, 4, 2], 0x04b4, 0x602a)],
                ),
            ],
        )
    }

    #[tokio::test]
    async fn watcher_events_are_published_as_usb_changed() {
        let events = EventBus::default();
        let mut sub = events.subscribe();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let forwarder = tokio::spawn(forward_usb_events(rx, events.clone()));

        // The FX2 re-enumeration: gone unprogrammed, back programmed.
        tx.send(UsbEvent::Removed(dev(&[3, 4, 2], 0x04b4, 0x602a)))
            .unwrap();
        tx.send(UsbEvent::Added(dev(&[3, 4, 2], 0x1d50, 0x608e)))
            .unwrap();
        drop(tx);
        forwarder.await.unwrap();

        for expected in [UsbChange::Removed, UsbChange::Added] {
            match sub.recv().await.unwrap() {
                Event::UsbChanged { device, change, .. } => {
                    assert_eq!(device, "3-3.4.2");
                    assert_eq!(change, expected);
                }
                other => panic!("expected usb.changed, got {other:?}"),
            }
        }
    }

    #[test]
    fn every_hub_lists_every_port_including_empty_ones() {
        let wire = to_wire(&bench(), &PortOverrides::default(), &HashMap::new());
        let numbers: Vec<u8> = wire.ports.iter().map(|p| p.number).collect();
        assert_eq!(numbers, vec![1, 2, 3, 4]);
        assert!(wire.ports[0].device.is_none() && wire.ports[1].device.is_none());
        assert_eq!(wire.ports[2].device.as_ref().unwrap().vid, 0x0925);
    }

    #[test]
    fn with_nothing_configured_instruments_and_empty_ports_are_switchable() {
        let wire = to_wire(&bench(), &PortOverrides::default(), &HashMap::new());
        for n in [0, 1, 2] {
            assert!(wire.ports[n].switchable, "port {} of 3-3", n + 1);
            assert!(!wire.ports[n].protected);
        }
    }

    #[test]
    fn a_port_feeding_a_hub_is_protected_by_default_with_its_reason() {
        let wire = to_wire(&bench(), &PortOverrides::default(), &HashMap::new());
        let port4 = &wire.ports[3];
        assert!(port4.power_switchable, "the hardware can switch it");
        assert!(port4.protected && !port4.switchable);
        assert_eq!(port4.user_choice, None, "automatic, not a user lock");
        assert!(port4
            .protection
            .as_deref()
            .unwrap()
            .contains("feeds hub 3-3.4"));
    }

    #[test]
    fn the_users_choice_overrides_protection_both_ways() {
        let mut ov = PortOverrides::default();
        ov.set("3-3", 4, Some(false)); // deliberately allow cutting the instrument hub
        ov.set("3-3", 3, Some(true)); // protect the logic analyzer
        let wire = to_wire(&bench(), &ov, &HashMap::new());
        assert!(wire.ports[3].switchable && wire.ports[3].user_choice == Some(false));
        assert!(!wire.ports[2].switchable && wire.ports[2].user_choice == Some(true));
        assert_eq!(
            wire.ports[2].protection.as_deref(),
            Some("locked by the user")
        );
    }

    #[test]
    fn a_switched_off_port_keeps_its_row_and_its_toggle() {
        let mut tree = bench();
        tree.children.retain(|c| port_of(c) != Some(3));
        let mut power = HashMap::new();
        power.insert(("3-3".to_string(), 3u8), Some(false));
        let port3 = &to_wire(&tree, &PortOverrides::default(), &power).ports[2];
        assert!(port3.device.is_none(), "unpowered device is off the bus");
        assert_eq!(port3.power, Some(false));
        assert!(
            port3.switchable,
            "the toggle to switch it back ON must survive"
        );
    }

    #[test]
    fn unreadable_hub_descriptor_is_a_hardware_limit_not_a_protection() {
        let mut tree = bench();
        tree.ppps = None;
        let port1 = &to_wire(&tree, &PortOverrides::default(), &HashMap::new()).ports[0];
        assert!(!port1.power_switchable && !port1.protected);
        assert!(port1
            .non_switchable_reason
            .as_deref()
            .unwrap()
            .contains("udev"));
    }

    #[tokio::test]
    async fn a_lock_is_persisted_and_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let ctl = RealUsbController::new(dir.path());
        let result = ctl.protect("99-1", 2, Some(true)).await.unwrap();
        assert!(result.protected && result.user_choice == Some(true));
        let restarted = RealUsbController::new(dir.path());
        assert_eq!(restarted.overrides().get("99-1", 2), Some(true));
        let cleared = restarted.protect("99-1", 2, None).await.unwrap();
        assert!(
            !cleared.protected && cleared.user_choice.is_none(),
            "back to automatic"
        );
    }

    #[test]
    fn instruments_are_flagged_including_the_reflashed_scope() {
        assert!(is_instrument(&dev(&[3, 4, 2], 0x04b4, 0x602a)));
        assert!(is_instrument(&dev(&[3, 4, 2], 0x1d50, 0x608e)));
        assert!(is_instrument(&dev(&[3, 3], 0x0925, 0x3881)));
        assert!(!is_instrument(&dev(&[3], 0x2109, 0x2822)));
    }

    #[test]
    fn every_empty_port_of_a_switchable_hub_gets_a_status_read() {
        let mut out = Vec::new();
        collect_empty_switchable(&bench(), &mut out);
        out.sort();
        assert_eq!(
            out,
            vec![
                ("3-3".into(), 1),
                ("3-3".into(), 2),
                ("3-3.4".into(), 1),
                ("3-3.4".into(), 3),
                ("3-3.4".into(), 4),
            ]
        );
    }
}
