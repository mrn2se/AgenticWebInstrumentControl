//! Which ports are protected, derived from what is plugged into them
//! (SPECIFICATION.md §4.4). Replaces the hand-written allowlist: nothing to
//! configure, and a device plugged into a formerly empty port is protected
//! the moment it appears.

use nusb::DeviceInfo;

/// USB class codes used by the protection rule.
const CLASS_HID: u8 = 0x03;
const CLASS_MASS_STORAGE: u8 = 0x08;
const CLASS_HUB: u8 = 0x09;
const CLASS_WIRELESS: u8 = 0xE0;
/// HID boot interface subclass and its keyboard/mouse protocols.
const HID_SUBCLASS_BOOT: u8 = 0x01;
const HID_PROTOCOL_KEYBOARD: u8 = 0x01;
const HID_PROTOCOL_MOUSE: u8 = 0x02;

/// Why the port holding `info` must not be switched without the user's say,
/// or `None` if nothing about it calls for protection.
pub fn derived_protection(info: &DeviceInfo) -> Option<String> {
    let interfaces: Vec<(u8, u8, u8)> = info
        .interfaces()
        .map(|i| (i.class(), i.subclass(), i.protocol()))
        .collect();
    let id = format!(
        "{}-{}",
        info.busnum(),
        info.port_chain()
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(".")
    );
    protection_from(&id, info.class(), &interfaces)
}

/// The rule itself, on plain class codes so it is testable without hardware.
/// `interfaces` are `(class, subclass, protocol)` triples.
pub(crate) fn protection_from(
    id: &str,
    device_class: u8,
    interfaces: &[(u8, u8, u8)],
) -> Option<String> {
    let any = |class: u8| device_class == class || interfaces.iter().any(|&(c, _, _)| c == class);
    if any(CLASS_HUB) {
        return Some(format!(
            "feeds hub {id}: switching it cuts every device behind it"
        ));
    }
    // Only boot-protocol HID — keyboards and mice. Many debug probes (e.g.
    // CMSIS-DAP v1) are *generic* HID devices; protecting all of HID would
    // lock exactly the programmers this feature exists to switch.
    let keyboard_or_mouse = interfaces.iter().any(|&(c, s, p)| {
        c == CLASS_HID
            && s == HID_SUBCLASS_BOOT
            && (p == HID_PROTOCOL_KEYBOARD || p == HID_PROTOCOL_MOUSE)
    });
    if keyboard_or_mouse {
        return Some("a keyboard or mouse is attached".to_string());
    }
    if any(CLASS_MASS_STORAGE) {
        return Some("a mass-storage device, possibly a disk in use, is attached".to_string());
    }
    if any(CLASS_WIRELESS) {
        return Some("a wireless controller (e.g. Bluetooth) is attached".to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hub_is_protected_because_it_feeds_everything_behind_it() {
        let reason = protection_from("3-3.4", CLASS_HUB, &[(CLASS_HUB, 0, 0)]).unwrap();
        assert!(reason.contains("feeds hub 3-3.4"), "{reason}");
    }

    #[test]
    fn boot_keyboards_and_mice_are_protected() {
        assert!(
            protection_from("x", 0, &[(CLASS_HID, 1, 1)]).is_some(),
            "keyboard"
        );
        assert!(
            protection_from("x", 0, &[(CLASS_HID, 1, 2)]).is_some(),
            "mouse"
        );
    }

    #[test]
    fn a_generic_hid_debug_probe_is_not_protected() {
        // CMSIS-DAP v1: HID, subclass 0, protocol 0 — a programmer to switch.
        assert_eq!(
            protection_from("x", 0, &[(CLASS_HID, 0, 0), (0xFF, 0, 0)]),
            None
        );
    }

    #[test]
    fn storage_and_bluetooth_are_protected() {
        assert!(protection_from("x", 0, &[(CLASS_MASS_STORAGE, 6, 0x50)]).is_some());
        assert!(protection_from("x", CLASS_WIRELESS, &[(CLASS_WIRELESS, 1, 1)]).is_some());
    }

    #[test]
    fn instruments_uarts_and_vendor_devices_are_switchable() {
        assert_eq!(
            protection_from("x", 0xFF, &[(0xFF, 0, 0)]),
            None,
            "fx2lafw logic analyzer / FX2"
        );
        assert_eq!(
            protection_from("x", 0x02, &[(0x02, 2, 1), (0x0A, 0, 0)]),
            None,
            "CDC-ACM UART"
        );
    }
}
