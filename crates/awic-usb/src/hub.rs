//! Standard USB hub-class control transfers (SPECIFICATION.md §4.2).
//!
//! Every transfer here targets endpoint 0 with `ControlType::Class`. None of
//! them claim an interface or touch the kernel `hub` driver, which is exactly
//! why this coexists with it: the kernel driver still owns the hub's
//! interface, and these are the same class requests the kernel driver itself
//! would issue.

use std::time::Duration;

use nusb::transfer::{ControlIn, ControlOut, ControlType, Recipient, TransferError};
use nusb::Device;

use crate::error::{Result, UsbError};

const GET_DESCRIPTOR: u8 = 0x06;
const CLEAR_FEATURE: u8 = 0x01;
const SET_FEATURE: u8 = 0x03;
const GET_STATUS: u8 = 0x00;

/// `wValue` for `SET_FEATURE`/`CLEAR_FEATURE` addressing `PORT_POWER`.
const PORT_POWER: u16 = 8;

/// `wValue` for `GET_DESCRIPTOR` — USB 2.0 hub descriptor (`bDescriptorType`
/// 0x29 in the high byte).
const HUB_DESCRIPTOR_USB2: u16 = 0x2900;
/// `wValue` for `GET_DESCRIPTOR` — USB 3.x ("SuperSpeed") hub descriptor
/// (`bDescriptorType` 0x2A in the high byte).
const HUB_DESCRIPTOR_USB3: u16 = 0x2A00;

/// bcdUSB threshold above which a hub is treated as SuperSpeed for the
/// purposes of picking a descriptor type and a port-status bit layout.
const USB3_BCD: u16 = 0x0300;

/// A control-transfer timeout generous enough for a hub that's momentarily
/// busy servicing another port, but short enough that a wedged hub doesn't
/// hang whoever is enumerating the tree.
const CONTROL_TIMEOUT: Duration = Duration::from_millis(1000);

/// Parsed `wHubCharacteristics` plus `bNbrPorts`, from `GET_DESCRIPTOR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HubDescriptor {
    pub num_ports: u8,
    /// `wHubCharacteristics` bits\[1:0\] == `0b01`: per-port power switching.
    /// `0b00` is ganged (all-or-nothing) power switching; other values exist
    /// but are not per-port either. SPECIFICATION.md §4.2.
    pub ppps: bool,
    /// Which descriptor type actually answered — determines the port-status
    /// bit layout used by [`parse_port_status`] (the `PORT_POWER` bit sits at
    /// a different offset for SuperSpeed hubs).
    pub superspeed: bool,
}

/// Reads and parses the hub descriptor (SPECIFICATION.md §4.2):
/// `bmRequestType 0xA0, bRequest 0x06, wValue 0x2900/0x2A00`.
///
/// `usb_version` is the hub's own `bcdUSB` (from its device descriptor),
/// used only to guess which descriptor type to try first — on a wrong guess
/// (a stalled transfer) the other type is tried before giving up, since the
/// bcdUSB/descriptor-type correspondence is a strong convention rather than
/// something the spec guarantees.
pub async fn get_hub_descriptor(device: &Device, usb_version: u16) -> Result<HubDescriptor> {
    let first_superspeed = usb_version >= USB3_BCD;
    match read_hub_descriptor(device, first_superspeed).await {
        Ok(desc) => Ok(desc),
        Err(UsbError::Transfer(TransferError::Stall)) => {
            read_hub_descriptor(device, !first_superspeed).await
        }
        Err(e) => Err(e),
    }
}

async fn read_hub_descriptor(device: &Device, superspeed: bool) -> Result<HubDescriptor> {
    let value = if superspeed {
        HUB_DESCRIPTOR_USB3
    } else {
        HUB_DESCRIPTOR_USB2
    };
    let data = device
        .control_in(
            ControlIn {
                control_type: ControlType::Class,
                recipient: Recipient::Device,
                request: GET_DESCRIPTOR,
                value,
                index: 0,
                length: 64,
            },
            CONTROL_TIMEOUT,
        )
        .await?;
    parse_hub_descriptor(&data, superspeed)
}

/// Parses a raw hub descriptor buffer. Split out from [`get_hub_descriptor`]
/// so the layout logic is unit-testable from a byte buffer with no device.
///
/// Layout (USB 2.0 §11.23.2.1, USB 3.x §10.13.2.1 — `wHubCharacteristics`
/// sits at the same offset in both):
///
/// ```text
/// offset  size  field
///  0       1    bDescLength
///  1       1    bDescriptorType
///  2       1    bNbrPorts
///  3       2    wHubCharacteristics (little-endian)
///  ...
/// ```
pub fn parse_hub_descriptor(data: &[u8], superspeed: bool) -> Result<HubDescriptor> {
    if data.len() < 5 {
        return Err(UsbError::ShortHubDescriptor(data.len()));
    }
    let num_ports = data[2];
    let characteristics = u16::from_le_bytes([data[3], data[4]]);
    let ppps = (characteristics & 0b11) == 0b01;
    Ok(HubDescriptor {
        num_ports,
        ppps,
        superspeed,
    })
}

/// Sets port power on or off (SPECIFICATION.md §4.2):
/// `CLEAR_FEATURE`/`SET_FEATURE`, `wValue 8` (`PORT_POWER`), `wIndex <port>`.
///
/// Callers must check [`HubDescriptor::ppps`] first — sending this to a
/// ganged-power hub does not fail, it just silently does nothing, which is
/// exactly the footgun SPECIFICATION.md §4.2 calls out. `awic-usb`'s
/// higher-level [`crate::power::set_port_power`] does that check; this
/// function is the raw primitive for callers (and tests) that already know
/// what they're doing.
pub async fn set_port_power(device: &Device, port: u8, on: bool) -> Result<()> {
    let request = if on { SET_FEATURE } else { CLEAR_FEATURE };
    device
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Other,
                request,
                value: PORT_POWER,
                index: u16::from(port),
                data: &[],
            },
            CONTROL_TIMEOUT,
        )
        .await?;
    Ok(())
}

/// The bits of `wPortStatus` this crate cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortStatus {
    pub connected: bool,
    pub enabled: bool,
    pub powered: bool,
}

/// Reads port status (SPECIFICATION.md §4.2): `GET_STATUS`, `wIndex <port>`.
pub async fn get_port_status(device: &Device, port: u8, superspeed: bool) -> Result<PortStatus> {
    let data = device
        .control_in(
            ControlIn {
                control_type: ControlType::Class,
                recipient: Recipient::Other,
                request: GET_STATUS,
                value: 0,
                index: u16::from(port),
                length: 4,
            },
            CONTROL_TIMEOUT,
        )
        .await?;
    parse_port_status(&data, superspeed)
}

/// Parses a `wPortStatus`/`wPortChange` pair. Split out for unit testing.
///
/// `PORT_POWER` sits at a different bit for USB2 vs SuperSpeed hubs:
/// - USB 2.0 (§11.24.2.7.1): bit 8.
/// - USB 3.x (§10.16.2.6): bit 9 (bits 5:8 are the SuperSpeed link-state
///   field that displaces it by one).
pub fn parse_port_status(data: &[u8], superspeed: bool) -> Result<PortStatus> {
    if data.len() < 2 {
        return Err(UsbError::ShortPortStatus(data.len()));
    }
    let status = u16::from_le_bytes([data[0], data[1]]);
    let power_bit = if superspeed { 1 << 9 } else { 1 << 8 };
    Ok(PortStatus {
        connected: status & 0x0001 != 0,
        enabled: status & 0x0002 != 0,
        powered: status & power_bit != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usb2_descriptor(num_ports: u8, characteristics: u16) -> Vec<u8> {
        let mut d = vec![9, 0x29, num_ports, 0, 0, 0, 0, 0, 0];
        d[3..5].copy_from_slice(&characteristics.to_le_bytes());
        d
    }

    #[test]
    fn ppps_true_when_characteristics_bits_are_01() {
        let data = usb2_descriptor(4, 0b0000_0001);
        let desc = parse_hub_descriptor(&data, false).unwrap();
        assert_eq!(desc.num_ports, 4);
        assert!(desc.ppps);
    }

    #[test]
    fn ppps_false_for_ganged_power() {
        let data = usb2_descriptor(4, 0b0000_0000);
        let desc = parse_hub_descriptor(&data, false).unwrap();
        assert!(!desc.ppps);
    }

    #[test]
    fn ppps_false_for_reserved_characteristic_bits() {
        // bits[1:0] == 0b10 and 0b11 are not "per-port" either.
        for bits in [0b10u16, 0b11u16] {
            let data = usb2_descriptor(4, bits);
            assert!(!parse_hub_descriptor(&data, false).unwrap().ppps);
        }
    }

    #[test]
    fn other_characteristic_bits_do_not_affect_ppps() {
        // Compound device bit, overcurrent bits etc. set alongside ppps.
        let data = usb2_descriptor(4, 0b1110_0101);
        assert!(parse_hub_descriptor(&data, false).unwrap().ppps);
    }

    #[test]
    fn short_descriptor_is_rejected() {
        let err = parse_hub_descriptor(&[9, 0x29, 4], false).unwrap_err();
        assert!(matches!(err, UsbError::ShortHubDescriptor(3)));
    }

    #[test]
    fn usb2_port_power_bit_is_8() {
        // connected + powered, USB2 layout.
        let status: u16 = 0x0001 | (1 << 8);
        let data = status.to_le_bytes();
        let st = parse_port_status(&data, false).unwrap();
        assert!(st.connected);
        assert!(st.powered);
        assert!(!st.enabled);
    }

    #[test]
    fn usb3_port_power_bit_is_9_not_8() {
        // Bit 8 alone (part of the SuperSpeed link-state field) must NOT be
        // read as PORT_POWER on a SuperSpeed hub.
        let status: u16 = 1 << 8;
        let data = status.to_le_bytes();
        let st = parse_port_status(&data, true).unwrap();
        assert!(!st.powered);

        let status: u16 = 1 << 9;
        let data = status.to_le_bytes();
        let st = parse_port_status(&data, true).unwrap();
        assert!(st.powered);
    }

    #[test]
    fn short_port_status_is_rejected() {
        let err = parse_port_status(&[0x01], false).unwrap_err();
        assert!(matches!(err, UsbError::ShortPortStatus(1)));
    }
}
