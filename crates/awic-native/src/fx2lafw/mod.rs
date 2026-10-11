//! The `fx2lafw` driver: an 8-channel digital logic analyzer (the common
//! low-cost one at `0925:3881`) built on a Cypress EZ-USB FX2, programmed via
//! [`crate::fx2`] on every enumeration (SPECIFICATION.md §2.4, §2.5).

mod backend;
mod driver;
mod protocol;

pub use backend::Fx2lafwBackend;
pub use driver::{AcquisitionStats, Fx2lafwInstrument, FIRMWARE_FILE};
pub use protocol::{FirmwareVersion, SAMPLE_RATES_HZ};
