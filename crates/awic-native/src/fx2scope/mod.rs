//! The `fx2scope` driver: a 2-channel analog oscilloscope (the FX2 USB
//! oscilloscope, bus path `3-3.4.2` on this bench) built on a Cypress EZ-USB
//! FX2, programmed via [`crate::fx2`] on every enumeration (SPECIFICATION.md
//! §2.4, §2.5).

mod backend;
mod driver;
mod protocol;

pub use backend::Fx2ScopeBackend;
pub use driver::{AcquisitionStats, Fx2ScopeInstrument, FIRMWARE_FILE};
pub use protocol::{MAX_SUSTAINED_RATE_HZ, RANGES, SAMPLE_RATES_HZ};
