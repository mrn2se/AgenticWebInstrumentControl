//! Prints the real USB topology tree (SPECIFICATION.md §4.1).
//!
//! Read-only enumeration works unprivileged. Learning `ppps` and per-port
//! power state needs to *open* each hub, which needs write access to its
//! `/dev/bus/usb/BBB/DDD` node — run under `sudo -n` if those columns show up
//! as "?" when run as a normal user:
//!
//! ```text
//! cargo run --example topology            # read-only info only
//! sudo -n cargo run --example topology    # full info, incl. ppps/power
//! ```

use awic_usb::{enumerate, BusTopology, TopologyNode};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let buses = match enumerate().await {
        Ok(buses) => buses,
        Err(e) => {
            eprintln!("topology enumeration failed: {e}");
            std::process::exit(1);
        }
    };

    if buses.is_empty() {
        println!("no USB buses found");
        return;
    }

    for bus in &buses {
        print_bus(bus);
    }
}

fn print_bus(bus: &BusTopology) {
    println!("Bus {:03}", bus.bus);
    let last = bus.roots.len().saturating_sub(1);
    for (i, root) in bus.roots.iter().enumerate() {
        print_node(root, "", i == last);
    }
}

fn print_node(node: &TopologyNode, prefix: &str, is_last: bool) {
    let connector = if is_last { "└── " } else { "├── " };
    println!(
        "{prefix}{connector}{:<10} {}",
        node.device_id.to_string(),
        describe(node, false)
    );

    let child_prefix = format!("{prefix}{}", if is_last { "    " } else { "│   " });
    let last_child = node.children.len().saturating_sub(1);
    for (i, child) in node.children.iter().enumerate() {
        print_child(child, &child_prefix, i == last_child);
    }
}

fn print_child(node: &TopologyNode, prefix: &str, is_last: bool) {
    let connector = if is_last { "└── " } else { "├── " };
    let port = node.device_id.port_path.last().copied().unwrap_or(0);
    let power = match (node.upstream_port_switchable, node.upstream_port_power) {
        (false, _) => " [not switchable]".to_string(),
        (true, Some(true)) => " [powered]".to_string(),
        (true, Some(false)) => " [OFF]".to_string(),
        (true, None) => " [power: ?]".to_string(),
    };
    println!(
        "{prefix}{connector}port {port} → {}{power}",
        describe(node, node.is_hub)
    );

    if node.is_hub {
        let child_prefix = format!("{prefix}{}", if is_last { "    " } else { "│   " });
        let last_child = node.children.len().saturating_sub(1);
        for (i, grandchild) in node.children.iter().enumerate() {
            print_child(grandchild, &child_prefix, i == last_child);
        }
    }
}

/// One-line summary of a node: VID:PID, product name (falling back to a
/// bench-specific label for devices with no `iProduct` string — several of
/// these hubs don't report one), and, for hubs, port count and `ppps`.
/// `include_id` additionally prefixes the node's own bus-path id, used for a
/// hub found deeper in the tree (a child line that is itself a hub, so its
/// own id is worth restating before its children are printed).
fn describe(node: &TopologyNode, include_id: bool) -> String {
    let name = node
        .product
        .clone()
        .unwrap_or_else(|| known_device_name(node.device_id.vid, node.device_id.pid));

    let mut s = String::new();
    if include_id {
        s.push_str(&node.device_id.to_string());
        s.push_str("  ");
    }
    s.push_str(&format!(
        "{:04x}:{:04x}  {name}",
        node.device_id.vid, node.device_id.pid
    ));

    if node.is_hub {
        let ports = node
            .num_ports
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".to_string());
        let ppps = match node.ppps {
            Some(true) => "ppps",
            Some(false) => "no ppps (ganged power)",
            None => "ppps unknown (couldn't open/read hub descriptor)",
        };
        s.push_str(&format!(", {ports} ports, {ppps}"));
    }

    s
}

/// Labels for the specific bench hardware this crate was verified against
/// (SPECIFICATION.md §4.1, §5.1) — cosmetic only, purely for this example;
/// `awic-usb` itself never hardcodes a device identity.
fn known_device_name(vid: u16, pid: u16) -> String {
    match (vid, pid) {
        (0x2109, 0x2822) => "VIA Labs hub".to_string(),
        (0x0bda, 0x5411) => "Realtek hub".to_string(),
        (0x0925, 0x3881) => {
            "fx2lafw logic analyzer (or FX2 oscilloscope in logic mode)".to_string()
        }
        (0x04b4, 0x602a) => "FX2 oscilloscope (unprogrammed)".to_string(),
        (0x1d50, 0x608e) => "FX2 oscilloscope (fx2lafw/fx2scope firmware loaded)".to_string(),
        _ => "unknown device".to_string(),
    }
}
