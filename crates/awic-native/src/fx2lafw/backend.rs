//! [`Backend`] impl for the `fx2lafw` driver (SPECIFICATION.md §2.2).

use std::sync::Arc;

use async_trait::async_trait;
use awic_core::{Backend, CoreError, DeviceId, Instrument, Result};

use crate::firmware::FirmwareResolver;

use super::driver::Fx2lafwInstrument;

/// `0925:3881` — the fx2lafw logic analyzer, and (SPECIFICATION.md §5.1) also
/// the FX2 oscilloscope in its logic-analyzer switch position. This backend
/// claims either: correct behaviour per §5.1 is to identify by bus path and
/// let the UI label by location, not to try to guess which physical device
/// is actually there.
const VENDOR_ID: u16 = 0x0925;
const PRODUCT_ID: u16 = 0x3881;

pub struct Fx2lafwBackend {
    firmware_resolver: Arc<FirmwareResolver>,
}

impl Fx2lafwBackend {
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

impl Default for Fx2lafwBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Backend for Fx2lafwBackend {
    fn name(&self) -> &str {
        "native-fx2lafw"
    }

    async fn discover(&self) -> Result<Vec<DeviceId>> {
        let devices = nusb::list_devices()
            .await
            .map_err(|e| CoreError::Device(e.to_string()))?;
        Ok(devices
            .filter(|d| d.vendor_id() == VENDOR_ID && d.product_id() == PRODUCT_ID)
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
        Ok(Box::new(Fx2lafwInstrument::new(
            id.clone(),
            self.firmware_resolver.clone(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_always_succeeds_regardless_of_hardware_presence() {
        // `open` just constructs the driver struct; it does no I/O (that's
        // `prepare()`'s job), so it must succeed even with no device
        // present, matching `awic-fake`'s `Backend::open` contract.
        let backend = Fx2lafwBackend::new();
        let id = DeviceId::new(3, vec![3, 3], VENDOR_ID, PRODUCT_ID);
        let instrument = backend.open(&id).await.unwrap();
        assert_eq!(instrument.id(), &id);
        assert!(!instrument.capabilities().is_empty());
    }

    #[tokio::test]
    async fn backend_name_identifies_the_native_driver() {
        assert_eq!(Fx2lafwBackend::new().name(), "native-fx2lafw");
    }
}
