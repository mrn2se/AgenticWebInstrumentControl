//! USB topology, hub port power and hot-plug detection (SPEC §4).
//!
//! Built on `nusb` (pure Rust, no C dependency) rather than `libusb`/`rusb`.
//! This crate is deliberately the only place in the workspace that names an
//! `nusb` type in its own public API — `awic-core` (DEVELOPMENT.md §1
//! rule 1) and everything downstream of it never need to.
//!
//! - [`topology`]: read the tree (SPECIFICATION.md §4.1).
//! - [`hub`]: the raw hub-class control transfers (SPECIFICATION.md §4.2).
//! - [`power`]: the *only* supported way to change port power — protection
//!   gated, delegating the check itself to `awic-core` (SPECIFICATION.md
//!   §4.4).
//! - [`watcher`]: hot-plug detection, netlink or polling (SPECIFICATION.md
//!   §4.3).

pub mod error;
pub mod hub;
pub mod power;
pub mod protect;
pub mod topology;
pub mod watcher;

pub use error::{Result, UsbError};
pub use protect::derived_protection;
pub use topology::{enumerate, BusTopology, TopologyNode};
pub use watcher::{spawn_from_config, DeviceWatcher, UsbEvent};
