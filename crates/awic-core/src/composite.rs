//! Several [`Backend`]s presented as one (SPECIFICATION.md §2.2).
//!
//! The server holds a single backend, while a real bench needs several — one
//! per native driver, plus the synthetic one for development without
//! hardware. This routes each device to the backend that discovered it. Like
//! everything in core it names no driver type: it only holds trait objects.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;

use crate::device_id::DeviceId;
use crate::error::{CoreError, Result};
use crate::instrument::{Backend, Instrument};

pub struct CompositeBackend {
    backends: Vec<Arc<dyn Backend>>,
    /// Which backend (by index) last discovered each device. Keyed by bus
    /// path — `DeviceId`'s identity — so an FX2 device that re-enumerates
    /// with a new VID:PID stays routed to its driver.
    owner: RwLock<HashMap<DeviceId, usize>>,
}

impl CompositeBackend {
    pub fn new(backends: Vec<Arc<dyn Backend>>) -> Self {
        Self {
            backends,
            owner: RwLock::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl Backend for CompositeBackend {
    fn name(&self) -> &str {
        "composite"
    }

    /// The union of every backend's devices. One backend failing — say, USB
    /// enumeration without permission — must not hide the others' devices,
    /// so failures are logged and skipped rather than propagated.
    async fn discover(&self) -> Result<Vec<DeviceId>> {
        let mut found = Vec::new();
        let mut owner = HashMap::new();
        for (index, backend) in self.backends.iter().enumerate() {
            match backend.discover().await {
                Ok(ids) => {
                    for id in ids {
                        owner.insert(id.clone(), index);
                        found.push(id);
                    }
                }
                Err(err) => tracing::warn!(backend = backend.name(), %err, "discovery failed"),
            }
        }
        *self.owner.write().expect("composite lock poisoned") = owner;
        Ok(found)
    }

    async fn open(&self, id: &DeviceId) -> Result<Box<dyn Instrument>> {
        let index = self
            .owner
            .read()
            .expect("composite lock poisoned")
            .get(id)
            .copied();
        match index {
            Some(index) => self.backends[index].open(id).await,
            None => Err(CoreError::DeviceNotFound(id.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AcqConfig, Capability, CaptureId, EffectiveAcq, SampleSink};

    struct Named(&'static str, Vec<DeviceId>);
    struct Opened(DeviceId, &'static str);

    #[async_trait]
    impl Instrument for Opened {
        fn id(&self) -> &DeviceId {
            &self.0
        }
        fn capabilities(&self) -> &[Capability] {
            &[]
        }
        async fn prepare(&mut self) -> Result<()> {
            Ok(())
        }
        async fn configure(&mut self, cfg: AcqConfig) -> Result<EffectiveAcq> {
            Ok(EffectiveAcq::digital(cfg))
        }
        async fn start(&mut self, _sink: SampleSink) -> Result<CaptureId> {
            Err(CoreError::Device(self.1.to_string()))
        }
        async fn stop(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl Backend for Named {
        fn name(&self) -> &str {
            self.0
        }
        async fn discover(&self) -> Result<Vec<DeviceId>> {
            if self.0 == "broken" {
                return Err(CoreError::Device("no permission".into()));
            }
            Ok(self.1.clone())
        }
        async fn open(&self, id: &DeviceId) -> Result<Box<dyn Instrument>> {
            Ok(Box::new(Opened(id.clone(), self.0)))
        }
    }

    fn id(path: &[u8], vid: u16, pid: u16) -> DeviceId {
        DeviceId::new(3, path.to_vec(), vid, pid)
    }

    async fn opened_by(c: &CompositeBackend, id: &DeviceId) -> String {
        let mut inst = c.open(id).await.unwrap();
        inst.start(SampleSink::channel(1).0)
            .await
            .unwrap_err()
            .to_string()
    }

    #[tokio::test]
    async fn routes_each_device_to_the_backend_that_found_it() {
        let logic = id(&[3, 3], 0x0925, 0x3881);
        let scope = id(&[3, 4, 2], 0x04b4, 0x602a);
        let c = CompositeBackend::new(vec![
            Arc::new(Named("fx2lafw", vec![logic.clone()])),
            Arc::new(Named("scope", vec![scope.clone()])),
        ]);
        assert_eq!(c.discover().await.unwrap().len(), 2);
        assert!(opened_by(&c, &logic).await.contains("fx2lafw"));
        assert!(opened_by(&c, &scope).await.contains("scope"));
    }

    #[tokio::test]
    async fn a_failing_backend_does_not_hide_the_others() {
        let logic = id(&[3, 3], 0x0925, 0x3881);
        let c = CompositeBackend::new(vec![
            Arc::new(Named("broken", vec![])),
            Arc::new(Named("fx2lafw", vec![logic.clone()])),
        ]);
        assert_eq!(c.discover().await.unwrap(), vec![logic]);
    }

    #[tokio::test]
    async fn reenumerated_device_stays_with_its_driver() {
        // Discovered unprogrammed; opened after the FX2 upload changed its PID.
        let c = CompositeBackend::new(vec![Arc::new(Named(
            "scope",
            vec![id(&[3, 4, 2], 0x04b4, 0x602a)],
        ))]);
        c.discover().await.unwrap();
        assert!(opened_by(&c, &id(&[3, 4, 2], 0x1d50, 0x608e))
            .await
            .contains("scope"));
    }

    #[tokio::test]
    async fn opening_an_undiscovered_device_is_a_clean_error() {
        let c = CompositeBackend::new(vec![]);
        assert!(matches!(
            c.open(&id(&[9], 1, 1)).await,
            Err(CoreError::DeviceNotFound(_))
        ));
    }
}
