//! [`Backend`] impl for the `fx2scope` driver (SPECIFICATION.md §2.2).

use std::sync::Arc;

use async_trait::async_trait;
use awic_core::{Backend, CoreError, DeviceId, Instrument, Result};

use crate::firmware::FirmwareResolver;

use super::driver::Fx2ScopeInstrument;

/// Unprogrammed FX2 oscilloscope, as it enumerates fresh off a power cycle
/// (SPECIFICATION.md §5.1; confirmed on this bench with `lsusb`).
const VID_UNPROGRAMMED: u16 = 0x04b4;
const PID_UNPROGRAMMED: u16 = 0x602a;

/// Post-firmware identity **as this exact firmware image actually presents
/// it** — `1d50:608e` ("OpenMoko, Inc. Fx2lafw"), confirmed with `lsusb -v`
/// on this bench after uploading `fx2lafw-hantek-6022bl.fw`. This is *not*
/// an early draft's assumed `04b5:602a` — see
/// [`super::protocol`]'s doc comment for the full ground-truth trail. Because
/// this VID:PID is the *generic* sigrok fx2lafw identity (shared by every
/// device this same base firmware family can drive), a device seen here is
/// only claimed as an FX2 oscilloscope if its `iSerial` string says so too — see
/// [`is_candidate`].
const VID_PROGRAMMED: u16 = 0x1d50;
const PID_PROGRAMMED: u16 = 0x608e;

/// The `iSerial` string this firmware reports, read on the bench via
/// `lsusb -v -d 1d50:608e` (`iSerial 3 Hantek 6022BL`). Matched
/// case-insensitively as a substring, not exact-compared: `libsigrok`'s
/// firmware-generation tooling embeds this string verbatim per device
/// profile, but this driver has no reason to assume its exact casing or
/// surrounding whitespace is contractual.
const PROGRAMMED_SERIAL_MARKER: &str = "hantek 6022bl";

/// Whether `info` looks like an FX2 oscilloscope this backend should claim:
/// either its known unprogrammed identity, or the generic post-firmware
/// identity *with* the matching serial string (see this module's doc
/// comment on why the serial check is required for the programmed case).
fn is_candidate(vid: u16, pid: u16, serial: Option<&str>) -> bool {
    if vid == VID_UNPROGRAMMED && pid == PID_UNPROGRAMMED {
        return true;
    }
    if vid == VID_PROGRAMMED && pid == PID_PROGRAMMED {
        return serial.is_some_and(|s| s.to_ascii_lowercase().contains(PROGRAMMED_SERIAL_MARKER));
    }
    false
}

pub struct Fx2ScopeBackend {
    firmware_resolver: Arc<FirmwareResolver>,
}

impl Fx2ScopeBackend {
    pub fn new() -> Self {
        Self {
            firmware_resolver: Arc::new(FirmwareResolver::default()),
        }
    }

    pub fn with_resolver(resolver: FirmwareResolver) -> Self {
        Self {
            firmware_resolver: Arc::new(resolver),
        }
    }
}

impl Default for Fx2ScopeBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Backend for Fx2ScopeBackend {
    fn name(&self) -> &str {
        "native-fx2scope"
    }

    async fn discover(&self) -> Result<Vec<DeviceId>> {
        let devices = nusb::list_devices()
            .await
            .map_err(|e| CoreError::Device(e.to_string()))?;
        Ok(devices
            .filter(|d| is_candidate(d.vendor_id(), d.product_id(), d.serial_number()))
            .map(|d| {
                DeviceId::new(
                    d.busnum(),
                    d.port_chain().to_vec(),
                    d.vendor_id(),
                    d.product_id(),
                )
            })
            .collect())
    }

    async fn open(&self, id: &DeviceId) -> Result<Box<dyn Instrument>> {
        Ok(Box::new(Fx2ScopeInstrument::new(
            id.clone(),
            self.firmware_resolver.clone(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_the_unprogrammed_identity_regardless_of_serial() {
        assert!(is_candidate(VID_UNPROGRAMMED, PID_UNPROGRAMMED, None));
        assert!(is_candidate(
            VID_UNPROGRAMMED,
            PID_UNPROGRAMMED,
            Some("anything")
        ));
    }

    #[test]
    fn claims_the_programmed_identity_only_with_the_matching_serial() {
        assert!(is_candidate(
            VID_PROGRAMMED,
            PID_PROGRAMMED,
            Some("Hantek 6022BL")
        ));
        // Case-insensitive, substring match.
        assert!(is_candidate(
            VID_PROGRAMMED,
            PID_PROGRAMMED,
            Some("HANTEK 6022BL")
        ));
    }

    #[test]
    fn does_not_claim_a_different_fx2lafw_device_at_the_generic_identity() {
        // The whole point of the serial check: `1d50:608e` is the *generic*
        // sigrok fx2lafw identity, shared by other devices this same
        // firmware family can drive.
        assert!(!is_candidate(VID_PROGRAMMED, PID_PROGRAMMED, None));
        assert!(!is_candidate(
            VID_PROGRAMMED,
            PID_PROGRAMMED,
            Some("CWAV USBee AX")
        ));
    }

    #[test]
    fn does_not_claim_unrelated_vid_pid() {
        assert!(!is_candidate(0x0925, 0x3881, Some("Hantek 6022BL")));
    }

    #[tokio::test]
    async fn open_always_succeeds_regardless_of_hardware_presence() {
        let backend = Fx2ScopeBackend::new();
        let id = DeviceId::new(3, vec![3, 4, 2], VID_UNPROGRAMMED, PID_UNPROGRAMMED);
        let instrument = backend.open(&id).await.unwrap();
        assert_eq!(instrument.id(), &id);
        assert!(!instrument.capabilities().is_empty());
    }

    #[tokio::test]
    async fn backend_name_identifies_the_native_driver() {
        assert_eq!(Fx2ScopeBackend::new().name(), "native-fx2scope");
    }
}
