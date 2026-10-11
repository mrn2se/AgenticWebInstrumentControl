//! `awic-core`'s error type.
//!
//! Every fallible core operation returns [`Result`]. In particular, the two
//! guardrails in [`crate::guardrails`] — USB port protection
//! (SPECIFICATION.md §4.4) and PSU soft limits (§7.1) — refuse with the named
//! variants here, which every transport (web, MCP, SCPI) inherits unchanged.

/// `awic-core`'s error type.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CoreError {
    /// SPECIFICATION.md §4.4: a `usb.power` request against a protected port
    /// — protected by what is attached (a hub, keyboard, disk…) or locked by
    /// the user. `reason` says which, so the refusal is actionable.
    #[error("hub {hub:?} port {port} is protected: {reason}")]
    PortProtected {
        hub: String,
        port: u8,
        reason: String,
    },

    /// SPECIFICATION.md §7.1: a PSU setpoint above the configured
    /// `[[psu.limits]]` for the device.
    #[error("{field} setpoint {value} exceeds soft limit {limit} for device {device:?}")]
    SoftLimitExceeded {
        device: String,
        field: String,
        value: f32,
        limit: f32,
    },

    #[error("device not found: {0:?}")]
    DeviceNotFound(String),

    #[error("capture not found: {0}")]
    CaptureNotFound(String),

    /// The capture existed and the store dropped it to stay within its
    /// bounds (B-37). The payload is the whole explanation.
    #[error("{0}")]
    CaptureEvicted(String),

    /// SPECIFICATION.md §5.4: `capture.measure` addressed a channel the
    /// capture does not have.
    #[error("channel {channel} not found in capture {capture} ({valid})")]
    ChannelNotFound {
        capture: String,
        channel: u8,
        /// Human-readable description of what *is* valid, e.g. "2 analog
        /// channels" or "digital channels are bits 0-7 of the packed byte".
        valid: String,
    },

    /// A driver-level failure, opaque to core by design — core never names a
    /// driver-specific error type (DEVELOPMENT.md §1 rule 1).
    #[error("device error: {0}")]
    Device(String),

    #[error("invalid configuration: {0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;
