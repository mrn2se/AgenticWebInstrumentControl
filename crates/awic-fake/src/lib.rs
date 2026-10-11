//! `awic-fake` — the synthetic `Backend` (SPECIFICATION.md §2.2, §14).
//!
//! Not a throwaway stub: this is the harness the wire protocol, UI, MCP and
//! SCPI surfaces are developed and CI-tested against with no hardware
//! attached. It implements `awic-core`'s [`Backend`]
//! and [`Instrument`] traits exactly as `awic-native` will, so a caller
//! cannot tell the two apart except by asking [`Backend::name`].
//!
//! - [`AnalogInstrument`] / [`DigitalInstrument`] generate samples as a pure
//!   function of absolute sample index, which is what lets an edge trigger
//!   be placed exactly (`trigger` module) and noise stay reproducible
//!   (`noise` module) even though sample generation is chunked and paced
//!   (`streaming` module) rather than computed in one shot.
//! - Analog samples are raw 8-bit device values plus scale/offset
//!   ([`analog::quantize`], [`analog::analog_scale_offset`]), matching how
//!   `awic-core::CaptureData::Analog` and the real driver both work.
//! - Digital sources ([`digital::UartSource`], [`digital::I2cSource`]) are
//!   built to be genuinely decodable, not just plausible-looking, since
//!   T-M5's protocol decoders run against them.

pub mod analog;
pub mod backend;
pub mod device;
pub mod digital;
mod noise;
mod streaming;
mod trigger;

pub use analog::{
    analog_scale_offset, quantize, AnalogChannelConfig, AnalogInstrument, AnalogWaveform,
};
pub use backend::FakeBackend;
pub use device::{
    AnalogDeviceSpec, DigitalDeviceSpec, FAKE_ANALOG_PID, FAKE_BUS, FAKE_DIGITAL_PID, FAKE_VID,
};
pub use digital::{DigitalChannelSource, DigitalInstrument, I2cLine, I2cSource, UartSource};
pub use trigger::trigger_target_index;
