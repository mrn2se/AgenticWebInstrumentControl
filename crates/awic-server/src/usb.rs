//! The USB control-plane trait this server dispatches `usb.tree` and
//! `usb.power` through (SPECIFICATION.md §4).
//!
//! `awic-usb` (topology enumeration, hub port power, SPEC §4.1/§4.2) is being
//! built in parallel and is currently an empty stub crate, so this crate does
//! not depend on its contents. Instead
//! it defines the narrow trait it actually needs and ships
//! [`UnimplementedUsbController`], which answers every call with a clean,
//! explicit "not implemented" error rather than silently doing nothing.
//! `main.rs` wires the real `awic-usb`-backed implementation in behind this
//! same trait once that crate lands — no other code in this crate changes.

use async_trait::async_trait;
use awic_proto::{UsbLabelResult, UsbPowerResult, UsbProtectResult, UsbTreeResult};

use crate::error::UsbOpError;

/// USB topology enumeration and hub port power control (SPECIFICATION.md
/// §4.1, §4.2). Port protection (§4.4) is enforced inside
/// [`Self::power`] through `awic-core`'s
/// [`awic_core::check_port_unprotected`], against what is attached at the
/// moment of switching.
#[async_trait]
pub trait UsbController: Send + Sync {
    /// Enumerates the current USB topology (SPECIFICATION.md §4.1).
    async fn tree(&self) -> Result<UsbTreeResult, UsbOpError>;

    /// Switches `hub`'s `port` on or off (SPECIFICATION.md §4.2). Only
    /// Refuses a protected port with the `port_protected` code.
    async fn power(&self, hub: &str, port: u8, on: bool) -> Result<UsbPowerResult, UsbOpError>;

    /// Locks (`Some(true)`), unlocks (`Some(false)`) or returns to automatic
    /// (`None`) a port's protection, persisting the choice as session state.
    async fn protect(
        &self,
        hub: &str,
        port: u8,
        protected: Option<bool>,
    ) -> Result<UsbProtectResult, UsbOpError>;

    /// Names what a port feeds and marks it as the device under test (live
    /// test F-08), persisting it as session state like a lock. `label: None`
    /// clears the name; `dut: None` keeps the mark. Controllers without
    /// session state refuse.
    async fn label(
        &self,
        _hub: &str,
        _port: u8,
        _label: Option<String>,
        _dut: Option<bool>,
    ) -> Result<UsbLabelResult, UsbOpError> {
        Err(UsbOpError::not_implemented("usb.label"))
    }
}

/// The stub used until `awic-usb` is wired in. Every operation refuses with
/// [`UsbOpError::not_implemented`] — never a silent success, an empty tree,
/// or a panic.
pub struct UnimplementedUsbController;

#[async_trait]
impl UsbController for UnimplementedUsbController {
    async fn tree(&self) -> Result<UsbTreeResult, UsbOpError> {
        Err(UsbOpError::not_implemented("usb.tree"))
    }

    async fn power(&self, _hub: &str, _port: u8, _on: bool) -> Result<UsbPowerResult, UsbOpError> {
        Err(UsbOpError::not_implemented("usb.power"))
    }

    async fn protect(
        &self,
        _hub: &str,
        _port: u8,
        _protected: Option<bool>,
    ) -> Result<UsbProtectResult, UsbOpError> {
        Err(UsbOpError::not_implemented("usb.protect"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unimplemented_controller_refuses_tree_cleanly() {
        let err = UnimplementedUsbController.tree().await.unwrap_err();
        assert_eq!(err.code, "not_implemented");
    }

    #[tokio::test]
    async fn unimplemented_controller_refuses_power_cleanly() {
        let err = UnimplementedUsbController
            .power("3-3", 1, true)
            .await
            .unwrap_err();
        assert_eq!(err.code, "not_implemented");
    }
}
