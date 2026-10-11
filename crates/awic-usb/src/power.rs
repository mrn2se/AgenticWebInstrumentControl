//! Protection-gated hub port power control (SPECIFICATION.md §4.4).
//!
//! [`set_port_power`] is the *only* entry point this crate offers for
//! changing port power, and it calls `awic-core`'s
//! [`awic_core::check_port_unprotected`] before touching hardware. There is
//! deliberately no lower-level "just send the transfer" function exported
//! outside this crate (the primitives in [`crate::hub`] are `pub` for
//! testing and reuse *within* awic-usb, but nothing in this crate's public
//! API reaches them without going through this check first) — so a caller
//! cannot route around the guardrail through `awic-usb`.

use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use awic_core::{check_port_unprotected, DeviceId, PortOverrides};

use crate::error::{Result, UsbError};
use crate::hub;

/// Switches `port` on `hub` (a bus-path string, e.g. `"3-3.4"`, as
/// [`DeviceId`]'s `Display` writes it) on or off, after confirming:
///
/// 1. the port is not protected (SPECIFICATION.md §4.4) — derived from what is
///    attached *right now*, read fresh from the bus rather than from any
///    cached tree, unless the user's lock/unlock choice in `overrides`
///    decides it outright, in which case nothing is enumerated;
/// 2. the hub is actually present on the bus;
/// 3. the hub's own descriptor advertises per-port power switching (`ppps`)
///    — SPECIFICATION.md §4.2's "report non-switchable rather than issuing a
///    transfer that silently does nothing";
/// 4. `port` is within the hub's `bNbrPorts`.
///
/// Returns the port's status read back after the change.
pub async fn set_port_power(
    overrides: &PortOverrides,
    hub: &str,
    port: u8,
    on: bool,
) -> Result<hub::PortStatus> {
    let hub_id = DeviceId::from_str(hub).map_err(|e| UsbError::InvalidHubId(hub.to_string(), e))?;
    let user = overrides.get(hub, port);
    // A user's lock or unlock decides on its own, whatever is attached
    // (`port_protection`), so only "automatic" enumerates: a lock refuses
    // even when the bus cannot be read, as `usb.protect` decides.
    let derived = match user {
        Some(_) => None,
        None => attached_protection(&hub_id, port).await?,
    };
    check_port_unprotected(hub, port, derived.as_deref(), user)?;

    let info = find_device(&hub_id).await?;
    let device = info.open().await.map_err(|source| UsbError::Open {
        hub: hub.to_string(),
        source,
    })?;

    let desc = hub::get_hub_descriptor(&device, info.usb_version()).await?;
    if !desc.ppps {
        return Err(UsbError::NotPowerSwitchable {
            hub: hub.to_string(),
        });
    }
    if port == 0 || port > desc.num_ports {
        return Err(UsbError::InvalidPort {
            hub: hub.to_string(),
            port,
            num_ports: desc.num_ports,
        });
    }

    match write_sysfs_disable(Path::new(SYSFS_USB_DEVICES), hub, port, on) {
        Ok(()) => tracing::debug!(hub, port, on, "port power set via sysfs"),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            // Kernel predates the per-port `disable` attribute (< 6.0).
            tracing::debug!(
                hub,
                port,
                "no sysfs port disable attribute; using control transfer"
            );
            hub::set_port_power(&device, port, on).await?;
        }
        Err(err) => {
            // Present but not writable: rootless container with a read-only
            // /sys, or udev rules not granting the attribute. The raw
            // transfer below is the degraded path — see the module note on
            // why it can leave the kernel holding a dead device.
            tracing::warn!(
                hub, port, %err,
                "cannot write sysfs port disable; falling back to a raw control transfer, \
                 which on Linux can leave a powered-off device enumerated. \
                 Fix: install tools/udev/99-awic.rules, or run rootful."
            );
            hub::set_port_power(&device, port, on).await?;
        }
    }
    hub::get_port_status(&device, port, desc.superspeed).await
}

/// Why the device currently on `hub`'s `port` should be protected, from a
/// fresh enumeration — `None` for an empty port or a device safe to switch.
pub async fn attached_protection(hub_id: &DeviceId, port: u8) -> Result<Option<String>> {
    let mut path = hub_id.port_path.clone();
    path.push(port);
    let devices = nusb::list_devices().await.map_err(UsbError::Enumeration)?;
    Ok(devices
        .into_iter()
        .find(|d| d.busnum() == hub_id.bus && d.port_chain() == path.as_slice())
        .and_then(|d| crate::protect::derived_protection(&d)))
}

/// Root of the kernel's USB device tree in sysfs.
const SYSFS_USB_DEVICES: &str = "/sys/bus/usb/devices";

/// Switches a port through the kernel's per-port `disable` attribute
/// (Linux >= 6.0): `…/<hub>:<config>.0/<hub>-port<N>/disable`, `1` = off.
///
/// **This is the primary mechanism on Linux, not an optimisation.** A raw
/// `CLEAR_FEATURE(PORT_POWER)` sent around the kernel flips the hub's port
/// power bit, but the kernel's hub driver never processes the disconnect:
/// verified on this bench (kernel 7.0, Realtek RTS5411), the oscilloscope stayed
/// enumerated for as long as the port was "off", its hub status reading
/// `0000 off` the whole time. `uhubctl --nosysfs` reproduces exactly that;
/// plain `uhubctl`, which uses this attribute, does not. Going through sysfs
/// asks the kernel to do it, so the kernel's view and the hardware agree.
fn write_sysfs_disable(root: &Path, hub: &str, port: u8, on: bool) -> io::Result<()> {
    std::fs::write(
        sysfs_disable_path(root, hub, port),
        if on { "0" } else { "1" },
    )
}

/// A hub's port attributes live under its interface, `<hub>:<config>.0`.
/// Hubs have a single interface; the configuration is read rather than
/// assumed, defaulting to `1` (every hub on this bench) if unreadable.
fn sysfs_disable_path(root: &Path, hub: &str, port: u8) -> PathBuf {
    let config = std::fs::read_to_string(root.join(hub).join("bConfigurationValue"))
        .ok()
        .and_then(|v| v.trim().parse::<u8>().ok())
        .unwrap_or(1);
    root.join(format!("{hub}:{config}.0"))
        .join(format!("{hub}-port{port}"))
        .join("disable")
}

/// Reads back a port's current status without changing it. Reading is
/// harmless, so it is not protection-gated.
pub async fn get_port_power(hub: &str, port: u8) -> Result<hub::PortStatus> {
    let hub_id = DeviceId::from_str(hub).map_err(|e| UsbError::InvalidHubId(hub.to_string(), e))?;
    let info = find_device(&hub_id).await?;
    let device = info.open().await.map_err(|source| UsbError::Open {
        hub: hub.to_string(),
        source,
    })?;
    let desc = hub::get_hub_descriptor(&device, info.usb_version()).await?;
    hub::get_port_status(&device, port, desc.superspeed).await
}

async fn find_device(id: &DeviceId) -> Result<nusb::DeviceInfo> {
    let devices = nusb::list_devices().await.map_err(UsbError::Enumeration)?;
    devices
        .into_iter()
        .find(|d| d.busnum() == id.bus && d.port_chain() == id.port_path.as_slice())
        .ok_or_else(|| UsbError::HubNotFound(id.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lays out a fake sysfs tree for hub `3-3.4` with a writable port 2.
    fn fake_sysfs(config: Option<&str>) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let cfg = config.unwrap_or("1");
        let port = root.path().join(format!("3-3.4:{cfg}.0/3-3.4-port2"));
        std::fs::create_dir_all(&port).unwrap();
        std::fs::write(port.join("disable"), "0").unwrap();
        if let Some(c) = config {
            std::fs::create_dir_all(root.path().join("3-3.4")).unwrap();
            std::fs::write(
                root.path().join("3-3.4/bConfigurationValue"),
                format!("{c}\n"),
            )
            .unwrap();
        }
        root
    }

    #[test]
    fn off_writes_1_and_on_writes_0_to_the_kernel_disable_attribute() {
        let root = fake_sysfs(Some("1"));
        let attr = root.path().join("3-3.4:1.0/3-3.4-port2/disable");
        write_sysfs_disable(root.path(), "3-3.4", 2, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(&attr).unwrap(),
            "1",
            "off = disable"
        );
        write_sysfs_disable(root.path(), "3-3.4", 2, true).unwrap();
        assert_eq!(std::fs::read_to_string(&attr).unwrap(), "0", "on = enable");
    }

    #[test]
    fn disable_path_follows_the_hubs_actual_configuration() {
        let root = fake_sysfs(Some("2"));
        assert!(
            sysfs_disable_path(root.path(), "3-3.4", 2).ends_with("3-3.4:2.0/3-3.4-port2/disable")
        );
        write_sysfs_disable(root.path(), "3-3.4", 2, false).unwrap();
    }

    #[test]
    fn missing_attribute_reports_not_found_so_the_caller_falls_back() {
        // A pre-6.0 kernel: no per-port disable attribute at all.
        let root = tempfile::tempdir().unwrap();
        let err = write_sysfs_disable(root.path(), "3-3.4", 2, false).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    /// A user-locked port is refused before any attempt to reach the hub —
    /// hardware-free: bus 99 does not exist, so nothing is attached and the
    /// only protection in play is the lock.
    #[tokio::test]
    async fn a_user_locked_port_is_refused_before_touching_hardware() {
        let mut overrides = PortOverrides::default();
        overrides.set("99-1", 2, Some(true));
        let err = set_port_power(&overrides, "99-1", 2, false)
            .await
            .unwrap_err();
        match err {
            UsbError::Guardrail(awic_core::CoreError::PortProtected { reason, .. }) => {
                assert!(reason.contains("locked by the user"), "{reason}");
            }
            other => panic!("expected a protection refusal, got {other:?}"),
        }
    }

    /// With nothing attached and no lock, protection lets the request through
    /// to the hardware checks — which then fail cleanly for a hub that is not
    /// there, rather than being refused as protected.
    #[tokio::test]
    async fn an_unlocked_empty_port_passes_protection() {
        let err = set_port_power(&PortOverrides::default(), "99-1", 2, true)
            .await
            .unwrap_err();
        assert!(
            !matches!(err, UsbError::Guardrail(_)),
            "unexpected refusal: {err:?}"
        );
    }
}
