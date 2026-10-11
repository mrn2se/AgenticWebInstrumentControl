//! Device identity (SPECIFICATION.md §2.3).

use std::fmt;
use std::hash::{Hash, Hasher};
use std::str::FromStr;

/// Identity that survives re-enumeration.
///
/// Device *numbers* change on every power cycle **and** every firmware
/// upload; VID:PID changes too, across an FX2 firmware upload (§2.4) and, for
/// the FX2 oscilloscope's switch-selected `0925:3881` mode, VID:PID is not even
/// unique on the bench (§5.1). The bus path — `bus` plus `port_path` — is the
/// only thing that stays put, so it is the only thing this type treats as
/// identity: [`PartialEq`], [`Eq`] and [`Hash`] deliberately ignore `vid` and
/// `pid`. A `Registry` keyed on `DeviceId` therefore recognizes "the same
/// device" across a `flashing` -> `ready` transition even though its VID:PID
/// changed underneath it.
#[derive(Debug, Clone)]
pub struct DeviceId {
    pub bus: u8,
    /// `[3, 4, 2]` displays as `"3-3.4.2"`.
    pub port_path: Vec<u8>,
    pub vid: u16,
    pub pid: u16,
}

impl DeviceId {
    pub fn new(bus: u8, port_path: Vec<u8>, vid: u16, pid: u16) -> Self {
        Self {
            bus,
            port_path,
            vid,
            pid,
        }
    }

    /// Returns a copy with updated VID:PID. Used when a device re-enumerates
    /// at the same bus path (e.g. after the FX2 firmware upload in §2.4)
    /// with a new product ID.
    #[must_use]
    pub fn with_vid_pid(mut self, vid: u16, pid: u16) -> Self {
        self.vid = vid;
        self.pid = pid;
        self
    }

    fn identity(&self) -> (u8, &[u8]) {
        (self.bus, &self.port_path)
    }
}

impl PartialEq for DeviceId {
    fn eq(&self, other: &Self) -> bool {
        self.identity() == other.identity()
    }
}

impl Eq for DeviceId {}

impl Hash for DeviceId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.identity().hash(state);
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-", self.bus)?;
        for (i, port) in self.port_path.iter().enumerate() {
            if i > 0 {
                write!(f, ".")?;
            }
            write!(f, "{port}")?;
        }
        Ok(())
    }
}

/// Error returned by [`DeviceId::from_str`].
///
/// Note this can only recover `bus` and `port_path` — VID:PID is not encoded
/// in the bus-path string, so a parsed `DeviceId` always carries `vid: 0,
/// pid: 0`. Use [`DeviceId::with_vid_pid`] to fill those in from other
/// context (e.g. a fresh USB enumeration) when they matter.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeviceIdParseError {
    #[error("empty device id")]
    Empty,
    #[error("missing '-' separating bus from port path in {0:?}")]
    MissingSeparator(String),
    #[error("invalid bus number in {0:?}")]
    InvalidBus(String),
    #[error("empty port path in {0:?}")]
    EmptyPortPath(String),
    #[error("invalid port number in {0:?}")]
    InvalidPort(String),
}

impl FromStr for DeviceId {
    type Err = DeviceIdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(DeviceIdParseError::Empty);
        }
        let (bus_str, path_str) = s
            .split_once('-')
            .ok_or_else(|| DeviceIdParseError::MissingSeparator(s.to_string()))?;
        let bus: u8 = bus_str
            .parse()
            .map_err(|_| DeviceIdParseError::InvalidBus(s.to_string()))?;
        if path_str.is_empty() {
            return Err(DeviceIdParseError::EmptyPortPath(s.to_string()));
        }
        let mut port_path = Vec::new();
        for part in path_str.split('.') {
            let port: u8 = part
                .parse()
                .map_err(|_| DeviceIdParseError::InvalidPort(s.to_string()))?;
            port_path.push(port);
        }
        Ok(DeviceId {
            bus,
            port_path,
            vid: 0,
            pid: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::hash_map::DefaultHasher;

    #[test]
    fn display_matches_spec_example() {
        let id = DeviceId::new(3, vec![3, 4, 2], 0x1d50, 0x608e);
        assert_eq!(id.to_string(), "3-3.4.2");
    }

    #[test]
    fn parse_display_round_trip() {
        for s in ["3-3.4.2", "1-1", "255-1.2.3.4.5"] {
            let id: DeviceId = s.parse().unwrap();
            assert_eq!(id.to_string(), s);
        }
    }

    #[test]
    fn parse_rejects_malformed_input() {
        assert_eq!("".parse::<DeviceId>(), Err(DeviceIdParseError::Empty));
        assert!(matches!(
            "no-dash-here-".parse::<DeviceId>(),
            Err(DeviceIdParseError::InvalidBus(_))
        ));
        assert!(matches!(
            "nodash".parse::<DeviceId>(),
            Err(DeviceIdParseError::MissingSeparator(_))
        ));
        assert!(matches!(
            "3-".parse::<DeviceId>(),
            Err(DeviceIdParseError::EmptyPortPath(_))
        ));
        assert!(matches!(
            "3-x.y".parse::<DeviceId>(),
            Err(DeviceIdParseError::InvalidPort(_))
        ));
    }

    #[test]
    fn equality_and_hash_ignore_vid_pid() {
        // Same bus path, different VID:PID — as happens across an FX2
        // firmware upload (unprogrammed -> programmed). Must compare equal.
        let before = DeviceId::new(3, vec![3, 4, 2], 0x04b4, 0x602a);
        let after = DeviceId::new(3, vec![3, 4, 2], 0x1d50, 0x608e);
        assert_eq!(before, after);

        fn hash_of(id: &DeviceId) -> u64 {
            let mut h = DefaultHasher::new();
            id.hash(&mut h);
            h.finish()
        }
        assert_eq!(hash_of(&before), hash_of(&after));
    }

    #[test]
    fn different_bus_paths_are_not_equal() {
        let a = DeviceId::new(3, vec![3, 4, 2], 0x1d50, 0x608e);
        let b = DeviceId::new(3, vec![3, 4, 3], 0x1d50, 0x608e);
        assert_ne!(a, b);
    }
}
