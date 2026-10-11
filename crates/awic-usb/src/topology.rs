//! USB topology enumeration (SPECIFICATION.md §4.1).
//!
//! Read-only enumeration (device/product strings, speed, VID:PID) needs no
//! privileges — it comes from `nusb::list_devices()`, which on Linux reads
//! sysfs. Learning a hub's `ppps` and a port's current power state needs to
//! *open* the hub (a `/dev/bus/usb/BBB/DDD` node), which needs write
//! permission; when that fails (e.g. running unprivileged), the affected
//! fields degrade to `None` rather than failing the whole enumeration — one
//! unreachable hub should not blank out the rest of the tree.

use std::collections::HashMap;

use awic_core::DeviceId;
use nusb::DeviceInfo;

use crate::hub;

/// `bDeviceClass` for a hub, per the USB standard class codes.
const USB_CLASS_HUB: u8 = 0x09;

/// One node of the topology tree: a hub or a leaf device, with the state of
/// the upstream port it occupies (SPECIFICATION.md §4.1). The root hub itself
/// is never a node — `nusb` excludes it from enumeration on Linux, and it
/// isn't part of the switchable-port model either (SPECIFICATION.md §4.4
/// switches only the ports of real, external hubs).
#[derive(Debug, Clone, PartialEq)]
pub struct TopologyNode {
    /// Identity built from the bus path (never the device number),
    /// SPECIFICATION.md §2.3.
    pub device_id: DeviceId,
    pub product: Option<String>,
    pub manufacturer: Option<String>,
    /// Formatted from `nusb::Speed` (e.g. `"High"`, `"Super"`) rather than
    /// leaking the `nusb` type — nothing outside this crate should need to
    /// depend on `nusb` to read this tree.
    pub speed: Option<String>,
    pub is_hub: bool,
    /// This hub's own `wHubCharacteristics` ppps bit — describes whether
    /// *this hub's downstream ports* (where its children sit) can be
    /// power-switched. `None` for non-hub nodes, and also `None` if the hub
    /// could not be opened or its descriptor could not be read.
    pub ppps: Option<bool>,
    /// This hub's port count (`bNbrPorts`), where known. `None` for non-hub
    /// nodes and for hubs whose descriptor couldn't be read.
    pub num_ports: Option<u8>,
    /// Whether the upstream port (the one on this node's *parent* that this
    /// node is plugged into) supports power switching. `false` for top-level
    /// nodes (parent is the root hub, out of scope for switching) and for
    /// any node whose parent hub isn't `ppps`.
    pub upstream_port_switchable: bool,
    /// Current power state of the upstream port, where knowable: `None` if
    /// the port isn't switchable, or if reading status failed.
    pub upstream_port_power: Option<bool>,
    /// Why the port this node occupies is protected by default, derived from
    /// what this node is (SPECIFICATION.md §4.4): `None` for instruments,
    /// programmers, UARTs and anything else safe to switch.
    pub derived_protection: Option<String>,
    pub children: Vec<TopologyNode>,
}

/// The topology of one USB bus: everything hanging off one root hub.
#[derive(Debug, Clone, PartialEq)]
pub struct BusTopology {
    pub bus: u8,
    pub roots: Vec<TopologyNode>,
}

/// A hub opened once during enumeration, kept around only long enough to
/// query the power status of each of its immediate children's ports.
struct OpenHub {
    device: nusb::Device,
    ppps: bool,
    superspeed: bool,
}

/// A node's position in the forest: `(bus, port_path)`, matching
/// [`DeviceId`]'s identity fields exactly.
type Key = (u8, Vec<u8>);

fn key_of(info: &DeviceInfo) -> Key {
    (info.busnum(), info.port_chain().to_vec())
}

fn parent_key(key: &Key) -> Option<Key> {
    if key.1.len() <= 1 {
        return None;
    }
    let mut parent = key.1.clone();
    parent.pop();
    Some((key.0, parent))
}

fn is_hub(info: &DeviceInfo) -> bool {
    info.class() == USB_CLASS_HUB
}

/// Removes `key` from `nodes` and recursively attaches its children (found
/// via `children_of`), returning the fully-assembled subtree. Requires
/// deepest-first-safe use: by the time a parent is attached, none of its
/// descendants may have been removed by anything else.
fn attach(
    key: &Key,
    nodes: &mut HashMap<Key, TopologyNode>,
    children_of: &HashMap<Key, Vec<Key>>,
) -> TopologyNode {
    let mut node = nodes
        .remove(key)
        .expect("every key in the forest has a node");
    if let Some(child_keys) = children_of.get(key) {
        let mut children: Vec<TopologyNode> = child_keys
            .iter()
            .map(|ck| attach(ck, nodes, children_of))
            .collect();
        children.sort_by_key(|c| c.device_id.port_path.last().copied().unwrap_or(0));
        node.children = children;
    }
    node
}

/// Enumerates every USB bus's topology (SPECIFICATION.md §4.1).
pub async fn enumerate() -> crate::error::Result<Vec<BusTopology>> {
    let infos: Vec<DeviceInfo> = nusb::list_devices()
        .await
        .map_err(crate::error::UsbError::Enumeration)?
        .collect();
    Ok(build_forest(infos).await)
}

/// Builds the forest from an already-collected device list. Split out from
/// [`enumerate`] so callers with their own enumeration (e.g. tests providing
/// a curated list — though building nodes still opens real hub devices, so
/// this remains hardware-dependent) can reuse the tree-building logic.
async fn build_forest(infos: Vec<DeviceInfo>) -> Vec<BusTopology> {
    // Pass 1 (async): visit shallowest-first so a node's parent hub, if any,
    // has already been opened and is available to supply upstream port
    // status. Flat maps keyed by bus path; assembled into a tree in pass 2.
    let mut by_key: HashMap<Key, DeviceInfo> = infos.into_iter().map(|i| (key_of(&i), i)).collect();
    let mut order: Vec<Key> = by_key.keys().cloned().collect();
    order.sort_by_key(|k| (k.0, k.1.len(), k.1.clone()));

    let mut open_hubs: HashMap<Key, OpenHub> = HashMap::new();
    let mut nodes: HashMap<Key, TopologyNode> = HashMap::new();

    for key in &order {
        let info = by_key.get(key).expect("key came from by_key's own keys");

        let (upstream_switchable, upstream_power) = match parent_key(key)
            .and_then(|pk| open_hubs.get(&pk))
        {
            Some(parent) if parent.ppps => {
                let port = *key.1.last().expect("non-root key has a port");
                match hub::get_port_status(&parent.device, port, parent.superspeed).await {
                    Ok(status) => (true, Some(status.powered)),
                    Err(e) => {
                        tracing::warn!(hub = ?parent_key(key), port, error = %e, "reading upstream port status failed");
                        (true, None)
                    }
                }
            }
            Some(_) => (false, None),
            None => (false, None),
        };

        let hub_flag = is_hub(info);
        let mut ppps = None;
        let mut num_ports = None;
        if hub_flag {
            match info.open().await {
                Ok(device) => match hub::get_hub_descriptor(&device, info.usb_version()).await {
                    Ok(desc) => {
                        ppps = Some(desc.ppps);
                        num_ports = Some(desc.num_ports);
                        open_hubs.insert(
                            key.clone(),
                            OpenHub {
                                device,
                                ppps: desc.ppps,
                                superspeed: desc.superspeed,
                            },
                        );
                    }
                    Err(e) => {
                        tracing::warn!(hub = ?key, error = %e, "reading hub descriptor failed");
                    }
                },
                Err(e) => {
                    tracing::warn!(hub = ?key, error = %e, "opening hub failed (needs write access to the device node)");
                }
            }
        }

        nodes.insert(
            key.clone(),
            TopologyNode {
                device_id: DeviceId::new(key.0, key.1.clone(), info.vendor_id(), info.product_id()),
                product: info.product_string().map(str::to_string),
                manufacturer: info.manufacturer_string().map(str::to_string),
                speed: info.speed().map(|s| format!("{s:?}")),
                is_hub: hub_flag,
                ppps,
                num_ports,
                upstream_port_switchable: upstream_switchable,
                upstream_port_power: upstream_power,
                derived_protection: crate::protect::derived_protection(info),
                children: Vec::new(),
            },
        );
    }
    by_key.clear();

    // Pass 2 (sync): assemble the tree, deepest nodes first so a node is
    // always attached to a parent that still exists in `nodes`.
    let mut children_of: HashMap<Key, Vec<Key>> = HashMap::new();
    let mut root_keys: Vec<Key> = Vec::new();
    for key in &order {
        match parent_key(key) {
            Some(pk) => children_of.entry(pk).or_default().push(key.clone()),
            None => root_keys.push(key.clone()),
        }
    }

    root_keys.sort();
    let mut by_bus: HashMap<u8, Vec<TopologyNode>> = HashMap::new();
    for key in &root_keys {
        let bus = key.0;
        let node = attach(key, &mut nodes, &children_of);
        by_bus.entry(bus).or_default().push(node);
    }

    let mut buses: Vec<BusTopology> = by_bus
        .into_iter()
        .map(|(bus, roots)| BusTopology { bus, roots })
        .collect();
    buses.sort_by_key(|b| b.bus);
    for bus in &mut buses {
        bus.roots.sort_by_key(|n| n.device_id.port_path.clone());
    }
    buses
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(bus: u8, path: &[u8], vid: u16, pid: u16) -> TopologyNode {
        TopologyNode {
            device_id: DeviceId::new(bus, path.to_vec(), vid, pid),
            product: None,
            manufacturer: None,
            speed: None,
            is_hub: false,
            ppps: None,
            num_ports: None,
            upstream_port_switchable: false,
            upstream_port_power: None,
            derived_protection: None,
            children: Vec::new(),
        }
    }

    /// `key_of`/`parent_key` are the only pieces of the tree-assembly
    /// algorithm that don't require opening a real device, so they're what
    /// gets exercised directly in a hardware-free unit test; the full
    /// `build_forest` async path is covered by the `examples/topology`
    /// hardware run instead (SPECIFICATION.md §4.1 has no fixture-friendly
    /// substitute for `nusb::DeviceInfo`, which this crate cannot construct
    /// without a real bus).
    #[test]
    fn parent_key_strips_the_last_port() {
        assert_eq!(parent_key(&(3, vec![3, 4, 2])), Some((3, vec![3, 4])));
        assert_eq!(parent_key(&(3, vec![3, 4])), Some((3, vec![3])));
        assert_eq!(parent_key(&(3, vec![3])), None);
    }

    #[test]
    fn attach_builds_a_tree_from_a_flat_map() {
        let mut nodes: HashMap<Key, TopologyNode> = HashMap::new();
        nodes.insert((3, vec![3]), leaf(3, &[3], 0x2109, 0x2822));
        nodes.insert((3, vec![3, 3]), leaf(3, &[3, 3], 0x0925, 0x3881));
        nodes.insert((3, vec![3, 4]), leaf(3, &[3, 4], 0x0bda, 0x5411));
        nodes.insert((3, vec![3, 4, 2]), leaf(3, &[3, 4, 2], 0x04b4, 0x602a));

        let mut children_of: HashMap<Key, Vec<Key>> = HashMap::new();
        children_of.insert((3, vec![3]), vec![(3, vec![3, 3]), (3, vec![3, 4])]);
        children_of.insert((3, vec![3, 4]), vec![(3, vec![3, 4, 2])]);

        let root = attach(&(3, vec![3]), &mut nodes, &children_of);
        assert_eq!(root.device_id, DeviceId::new(3, vec![3], 0, 0));
        assert_eq!(root.children.len(), 2);
        assert_eq!(root.children[0].device_id.port_path, vec![3, 3]);
        assert_eq!(root.children[1].device_id.port_path, vec![3, 4]);
        assert_eq!(
            root.children[1].children[0].device_id.port_path,
            vec![3, 4, 2]
        );
        assert!(nodes.is_empty());
    }
}
