//! Native Rust drivers for FX2-based instruments (SPECIFICATION.md §2.4,
//! §2.5): the `fx2lafw` and `fx2scope` drivers, sharing [`fx2`]'s uploader.
//!
//! # Licensing note
//!
//! AWIC is MPL-2.0; `libsigrok` and the `sigrok-firmware-fx2lafw`
//! images are GPL. Where this crate's doc comments cite `libsigrok` source
//! (`src/ezusb.c`, `src/hardware/fx2lafw/{protocol.h,protocol.c,api.c}`),
//! that is a *protocol reference* consulted to confirm command numbers,
//! struct layouts and the sample-rate arithmetic — facts, which are not
//! copyrightable — cross-checked against the real hardware wherever this
//! crate could reach it (see each module's doc comment for exactly what was
//! verified on the bench vs. taken from documentation). No GPL source was
//! copied, translated line-by-line, or paraphrased into this crate's code.
//!
//! # Crate layout
//!
//! - [`error`]: this crate's error type, converted to
//!   `awic_core::CoreError::Device` at the `Instrument`/`Backend` trait
//!   boundary (core names no driver-specific type, DEVELOPMENT.md §1
//!   rule 1).
//! - [`firmware`]: firmware blobs and the SPECIFICATION.md §13 resolution
//!   chain (host mount, baked-in, auto-download-not-yet-implemented).
//! - [`fx2`]: the shared FX2 firmware uploader (SPECIFICATION.md §2.4),
//!   reused by every FX2-based driver.
//! - [`fx2lafw`]: the fx2lafw logic analyzer digital logic analyzer driver.
//! - [`fx2scope`]: the FX2 2-channel analog oscilloscope driver.

pub mod error;
pub mod firmware;
pub mod fx2;
pub mod fx2lafw;
pub mod fx2scope;

pub use error::{NativeError, Result};
pub use firmware::{Firmware, FirmwareResolver, FirmwareSource, ResolvedFirmware};
