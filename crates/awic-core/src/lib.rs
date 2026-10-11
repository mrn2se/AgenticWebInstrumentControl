//! `awic-core` — traits, registry, capture store, event bus, config, and the
//! two safety guardrails (SPECIFICATION.md §2, §4.4, §7.1).
//!
//! This crate names **no driver-specific type** anywhere in its public API —
//! not `nusb`, not any FX2 type. `awic-native` and `awic-fake`
//! both implement [`Backend`] and [`Instrument`] against this crate alone; a
//! third backend could be added without touching a single line here
//! (DEVELOPMENT.md §1 rule 1, SPECIFICATION.md §2.2).

pub mod capability;
pub mod capture_read;
pub mod capture_store;
pub mod composite;
pub mod config;
pub mod decimate;
pub mod device_id;
pub mod error;
pub mod event_bus;
/// Bench recordings for tests; see `testdata/bench/`.
#[cfg(any(test, feature = "fixtures"))]
pub mod fixtures;
pub mod guardrails;
pub mod instrument;
pub mod measure;
pub mod registry;
pub mod series;
pub mod threshold;

pub use capability::Capability;
pub use capture_read::{read_capture_channel, read_capture_window, ReadCaptureResult};
pub use capture_store::{
    Capture, CaptureData, CaptureId, CaptureIdParseError, CaptureMeta, CaptureStore,
    DecimatedSamples, NewAnalogCapture, NewDigitalCapture, Transition,
};
pub use composite::CompositeBackend;
pub use config::{Config, McpConfig};
pub use decimate::{bitwise_decimate, minmax_decimate};
pub use device_id::{DeviceId, DeviceIdParseError};
pub use error::{CoreError, Result};
pub use event_bus::EventBus;
pub use guardrails::{
    check_port_unprotected, check_soft_limits, port_protection, PortLabel, PortOverrides,
    PORT_OVERRIDES_FILE,
};
pub use instrument::{
    AcqConfig, Backend, ChannelCalibration, EffectiveAcq, Instrument, SampleChunk, SampleSink,
    Slope, Trigger,
};
pub use measure::measure_capture;
pub use registry::{DeviceState, Registry};
pub use series::{
    channel_clipping, channel_series, hardware_channel_clipping, hardware_channel_name,
    hardware_channel_series, hardware_channels, Clipping, Series,
};
