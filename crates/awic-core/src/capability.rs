//! Instrument capabilities (SPECIFICATION.md §2.3), and how they map onto the
//! wire-facing [`awic_proto::CapabilityInfo`] used in `device.list` results.

/// What an [`crate::Instrument`] can do. The graph module (SPEC §5.2) drives
/// its UI entirely from this: `Analog` yields channel/scale/offset controls,
/// `Digital` yields channel/threshold controls, with no per-model code.
#[derive(Debug, Clone, PartialEq)]
pub enum Capability {
    Analog {
        channels: u8,
        bits: u8,
        rates_hz: Vec<u64>,
        ranges_v: Vec<f32>,
    },
    Digital {
        channels: u8,
        max_rate_hz: u64,
        /// The driver's exact rate ladder, e.g. `fx2lafw`'s divisor-reachable
        /// rates up to its sustained maximum. Empty means only the ceiling
        /// is known.
        rates_hz: Vec<u64>,
    },
    PowerSupply {
        channels: u8,
        v_max: f32,
        i_max: f32,
    },
}

impl From<&Capability> for awic_proto::CapabilityInfo {
    fn from(cap: &Capability) -> Self {
        match cap.clone() {
            Capability::Analog {
                channels,
                bits,
                rates_hz,
                ranges_v,
            } => awic_proto::CapabilityInfo::Analog {
                channels,
                bits,
                rates_hz,
                ranges_v,
            },
            Capability::Digital {
                channels,
                max_rate_hz,
                rates_hz,
            } => awic_proto::CapabilityInfo::Digital {
                channels,
                max_rate_hz,
                rates_hz,
            },
            Capability::PowerSupply {
                channels,
                v_max,
                i_max,
            } => awic_proto::CapabilityInfo::PowerSupply {
                channels,
                v_max,
                i_max,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_to_wire_capability_info() {
        let cap = Capability::Analog {
            channels: 2,
            bits: 8,
            rates_hz: vec![48_000_000, 24_000_000],
            ranges_v: vec![5.0, 2.5, 1.0, 0.5],
        };
        let wire: awic_proto::CapabilityInfo = (&cap).into();
        assert_eq!(
            wire,
            awic_proto::CapabilityInfo::Analog {
                channels: 2,
                bits: 8,
                rates_hz: vec![48_000_000, 24_000_000],
                ranges_v: vec![5.0, 2.5, 1.0, 0.5],
            }
        );
    }
}
