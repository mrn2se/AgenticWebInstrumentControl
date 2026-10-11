//! A no-op [`Backend`] used until a real one (`awic-native`/`awic-fake`) is
//! wired into `main.rs`.
//!
//! Both real backends are built by other tasks in parallel; this crate talks
//! only to the `Backend`/`Instrument` traits from `awic-core`, never to
//! either concrete crate, so swapping this out later is a one-line change in
//! `main.rs` and nothing else.

use async_trait::async_trait;
use awic_core::{Backend, CoreError, DeviceId, Instrument, Result};

/// Reports no devices and refuses to open any. Lets the server start,
/// serve its static UI, and answer `device.list` (with an empty list)
/// before any real backend exists.
pub struct PlaceholderBackend;

#[async_trait]
impl Backend for PlaceholderBackend {
    fn name(&self) -> &str {
        "placeholder"
    }

    async fn discover(&self) -> Result<Vec<DeviceId>> {
        Ok(Vec::new())
    }

    async fn open(&self, id: &DeviceId) -> Result<Box<dyn Instrument>> {
        Err(CoreError::DeviceNotFound(id.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn discover_is_empty() {
        assert!(PlaceholderBackend.discover().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn open_always_fails_with_device_not_found() {
        let id = DeviceId::new(1, vec![1], 0, 0);
        match PlaceholderBackend.open(&id).await {
            Err(CoreError::DeviceNotFound(_)) => {}
            Err(other) => panic!("expected DeviceNotFound, got a different error: {other}"),
            Ok(_) => panic!("expected DeviceNotFound, got Ok"),
        }
    }
}
