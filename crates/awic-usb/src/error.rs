//! `awic-usb`'s error type.

use thiserror::Error;

/// Errors from topology enumeration, hub port power control, and device
/// watching. Driver-specific by design — this crate is where `nusb` errors
/// are allowed to appear (unlike `awic-core`, which names no driver type at
/// all, DEVELOPMENT.md §1 rule 1).
#[derive(Debug, Error)]
pub enum UsbError {
    #[error("usb enumeration failed: {0}")]
    Enumeration(#[source] nusb::Error),

    #[error("failed to open device {hub:?}: {source}")]
    Open {
        hub: String,
        #[source]
        source: nusb::Error,
    },

    #[error("control transfer failed: {0}")]
    Transfer(#[from] nusb::transfer::TransferError),

    #[error("malformed hub descriptor ({0} bytes)")]
    ShortHubDescriptor(usize),

    #[error("malformed port status ({0} bytes)")]
    ShortPortStatus(usize),

    #[error("hub {hub:?} does not support per-port power switching")]
    NotPowerSwitchable { hub: String },

    #[error("hub {hub:?} port {port} is out of range (hub reports {num_ports} ports)")]
    InvalidPort {
        hub: String,
        port: u8,
        num_ports: u8,
    },

    #[error("no hub found on the bus at {0:?}")]
    HubNotFound(String),

    #[error("{0:?} is not a valid hub identifier: {1}")]
    InvalidHubId(String, awic_core::DeviceIdParseError),

    /// SPECIFICATION.md §4.4: port protection, enforced by
    /// `awic-core`. `awic-usb` never bypasses this — see [`crate::power`].
    #[error(transparent)]
    Guardrail(#[from] awic_core::CoreError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, UsbError>;
