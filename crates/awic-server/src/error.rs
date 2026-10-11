//! Mapping from `awic-core` errors (and this crate's own stub errors) onto
//! the wire-facing `{ "code": ..., "msg": ... }` shape (SPECIFICATION.md
//! §3.1). Codes are stable strings a client can match on; a guardrail
//! refusal (USB port protection, PSU soft limits) always keeps its own code
//! here rather than collapsing into a generic "device_error".

use awic_core::CoreError;
use awic_proto::Response;

/// Turns a [`CoreError`] into the [`Response`] it should produce for the
/// request `id` that triggered it, preserving a stable machine-readable code
/// per variant (SPECIFICATION.md §3.1's error shape).
pub fn core_error_response(id: u32, err: CoreError) -> Response {
    let code = core_error_code(&err);
    Response::err(id, code, err.to_string())
}

pub(crate) fn core_error_code(err: &CoreError) -> &'static str {
    match err {
        CoreError::PortProtected { .. } => "port_protected",
        CoreError::SoftLimitExceeded { .. } => "soft_limit_exceeded",
        CoreError::DeviceNotFound(_) => "device_not_found",
        CoreError::CaptureNotFound(_) => "capture_not_found",
        CoreError::CaptureEvicted(_) => "capture_evicted",
        CoreError::ChannelNotFound { .. } => "channel_not_found",
        CoreError::Device(_) => "device_error",
        CoreError::Config(_) => "invalid_config",
    }
}

/// The error shape returned by [`crate::usb::UsbController`] operations,
/// deliberately not `awic_core::CoreError` — a USB op can fail for reasons
/// core never needs to model (e.g. "not implemented yet"), and this keeps
/// that failure mode from leaking into core's error enum
/// (DEVELOPMENT.md §1 rule 1: core names no driver-specific type).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbOpError {
    pub code: String,
    pub msg: String,
}

impl UsbOpError {
    /// The stub [`crate::usb::UnimplementedUsbController`]'s answer to every
    /// operation: a clean, explicit refusal rather than a silent no-op or a
    /// panic.
    pub fn not_implemented(op: &str) -> Self {
        Self {
            code: "not_implemented".to_string(),
            msg: format!("{op} requires awic-usb, which is not wired into this server build yet"),
        }
    }
}

impl From<UsbOpError> for (String, String) {
    fn from(err: UsbOpError) -> Self {
        (err.code, err.msg)
    }
}
