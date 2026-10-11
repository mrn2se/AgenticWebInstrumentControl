//! Synthetic device identities and capability specs (SPECIFICATION.md §2.3).
//!
//! The bus path is chosen so it can never collide with a real enumerated
//! device: Linux USB bus numbers are small (typically 1-a few dozen), so
//! `FAKE_BUS` sits far outside that range, and `FAKE_VID` (`0xFFFF`) is a
//! value the USB-IF explicitly never assigns to a real vendor.

use awic_core::{Capability, DeviceId};

/// Bus number no real host controller will ever enumerate.
pub const FAKE_BUS: u8 = 253;
/// USB-IF reserves `0xFFFF` as "no vendor ID assigned" — never issued to a
/// real device, so it doubles as an unambiguous "this is synthetic" marker.
pub const FAKE_VID: u16 = 0xFFFF;
pub const FAKE_ANALOG_PID: u16 = 0x0001;
pub const FAKE_DIGITAL_PID: u16 = 0x0002;

/// Capability + identity for the default fake analog device, mirroring the
/// real FX2 oscilloscope (SPECIFICATION.md §2.5's verified ground truth).
#[derive(Debug, Clone, PartialEq)]
pub struct AnalogDeviceSpec {
    pub id: DeviceId,
    pub label: String,
    pub channels: u8,
    pub bits: u8,
    pub rates_hz: Vec<u64>,
    pub ranges_v: Vec<f32>,
}

impl AnalogDeviceSpec {
    /// The default fake analog device: 2 channels, 8 bits, the FX2
    /// oscilloscope's verified rate ladder and voltage ranges.
    pub fn scope_2ch_like() -> Self {
        Self {
            id: DeviceId::new(FAKE_BUS, vec![1], FAKE_VID, FAKE_ANALOG_PID),
            label: "AWIC Fake Analog (2-channel oscilloscope)".to_string(),
            channels: 2,
            bits: 8,
            rates_hz: vec![
                48_000_000, 30_000_000, 24_000_000, 16_000_000, 8_000_000, 4_000_000, 1_000_000,
                500_000, 200_000, 100_000,
            ],
            ranges_v: vec![5.0, 2.5, 1.25, 0.5],
        }
    }

    pub fn capability(&self) -> Capability {
        Capability::Analog {
            channels: self.channels,
            bits: self.bits,
            rates_hz: self.rates_hz.clone(),
            ranges_v: self.ranges_v.clone(),
        }
    }
}

/// Capability + identity for the default fake digital device, mirroring the
/// fx2lafw logic analyzer (`fx2lafw`, SPECIFICATION.md §2.5).
#[derive(Debug, Clone, PartialEq)]
pub struct DigitalDeviceSpec {
    pub id: DeviceId,
    pub label: String,
    pub channels: u8,
    pub max_rate_hz: u64,
    pub rates_hz: Vec<u64>,
}

impl DigitalDeviceSpec {
    /// The default fake digital device: 8 channels, 24 MHz max rate, a
    /// rate ladder shaped like the real `fx2lafw` fx2lafw logic analyzer's so the UI
    /// exercises the same rate picker without hardware attached.
    pub fn logic_8ch_like() -> Self {
        Self {
            id: DeviceId::new(FAKE_BUS, vec![2], FAKE_VID, FAKE_DIGITAL_PID),
            label: "AWIC Fake Digital (8-channel logic analyzer)".to_string(),
            channels: 8,
            max_rate_hz: 24_000_000,
            rates_hz: vec![
                20_000, 25_000, 50_000, 100_000, 200_000, 250_000, 500_000, 1_000_000, 2_000_000,
                3_000_000, 4_000_000, 6_000_000, 8_000_000, 12_000_000, 16_000_000, 24_000_000,
            ],
        }
    }

    pub fn capability(&self) -> Capability {
        Capability::Digital {
            channels: self.channels,
            max_rate_hz: self.max_rate_hz,
            rates_hz: self.rates_hz.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_bus_path_cannot_collide_with_real_devices() {
        // Real topologies in this project's config (SPECIFICATION.md §4.4)
        // use single-digit bus numbers; guard against ever "fixing" this
        // constant into that range by accident. Checked at compile time
        // (`const { .. }`) since both sides are constants.
        const { assert!(FAKE_BUS > 32) };
        assert_eq!(FAKE_VID, 0xFFFF);
    }

    #[test]
    fn analog_and_digital_ids_are_distinct() {
        let a = AnalogDeviceSpec::scope_2ch_like();
        let d = DigitalDeviceSpec::logic_8ch_like();
        assert_ne!(a.id, d.id);
    }

    #[test]
    fn analog_capability_matches_scope_ground_truth() {
        let spec = AnalogDeviceSpec::scope_2ch_like();
        match spec.capability() {
            Capability::Analog {
                channels,
                bits,
                rates_hz,
                ranges_v,
            } => {
                assert_eq!(channels, 2);
                assert_eq!(bits, 8);
                assert_eq!(rates_hz.first(), Some(&48_000_000));
                assert_eq!(ranges_v, vec![5.0, 2.5, 1.25, 0.5]);
            }
            other => panic!("expected Analog capability, got {other:?}"),
        }
    }
}
