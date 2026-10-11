//! The synthetic [`Backend`] (SPECIFICATION.md §2.2, §14).
//!
//! Presents the two default fake devices — analog (FX2 oscilloscope-like) and
//! digital (fx2lafw logic analyzer-like) — so the wire protocol, UI, MCP and SCPI
//! surfaces all have something to talk to with no hardware attached.

use async_trait::async_trait;
use awic_core::{Backend, CoreError, DeviceId, Instrument, Result};

use crate::analog::AnalogInstrument;
use crate::device::{AnalogDeviceSpec, DigitalDeviceSpec};
use crate::digital::DigitalInstrument;

pub struct FakeBackend {
    analog_spec: AnalogDeviceSpec,
    digital_spec: DigitalDeviceSpec,
}

impl FakeBackend {
    pub fn new() -> Self {
        Self {
            analog_spec: AnalogDeviceSpec::scope_2ch_like(),
            digital_spec: DigitalDeviceSpec::logic_8ch_like(),
        }
    }

    /// Builds a backend presenting only the given device specs, for tests
    /// or configurations that want a non-default fake topology.
    pub fn with_specs(analog_spec: AnalogDeviceSpec, digital_spec: DigitalDeviceSpec) -> Self {
        Self {
            analog_spec,
            digital_spec,
        }
    }
}

impl Default for FakeBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Backend for FakeBackend {
    fn name(&self) -> &str {
        "fake"
    }

    async fn discover(&self) -> Result<Vec<DeviceId>> {
        Ok(vec![
            self.analog_spec.id.clone(),
            self.digital_spec.id.clone(),
        ])
    }

    async fn open(&self, id: &DeviceId) -> Result<Box<dyn Instrument>> {
        if *id == self.analog_spec.id {
            Ok(Box::new(AnalogInstrument::new(self.analog_spec.clone())))
        } else if *id == self.digital_spec.id {
            Ok(Box::new(DigitalInstrument::new(self.digital_spec.clone())))
        } else {
            Err(CoreError::DeviceNotFound(id.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn discover_lists_both_default_devices() {
        let backend = FakeBackend::new();
        let ids = backend.discover().await.unwrap();
        assert_eq!(ids.len(), 2);
    }

    #[tokio::test]
    async fn open_returns_matching_capability_for_each_id() {
        let backend = FakeBackend::new();
        let ids = backend.discover().await.unwrap();
        for id in ids {
            let instrument = backend.open(&id).await.unwrap();
            assert_eq!(instrument.id(), &id);
            assert!(!instrument.capabilities().is_empty());
        }
    }

    #[tokio::test]
    async fn open_rejects_an_unknown_device_id() {
        let backend = FakeBackend::new();
        let unknown = DeviceId::new(1, vec![9, 9], 0, 0);
        match backend.open(&unknown).await {
            Err(CoreError::DeviceNotFound(_)) => {}
            other => panic!("expected DeviceNotFound, got {}", other.is_ok()),
        }
    }
}
