//! `awic-sim` — the virtual bench (`[sim] enabled = true`, or `awic --sim`).
//!
//! A simulated board under test, with virtual instruments clipped onto its
//! test points, for trying AWIC — the web UI and the MCP tools — with no
//! hardware at all. Unlike `awic-fake`, which streams fixed patterns for
//! tests, this is a board with behaviour: firmware with a serial console
//! that answers commands, a sensor and a flash it talks to, a 555 timer, an
//! unmarked counter chip to identify, and faults to hunt down.
//!
//! - [`world`]: the board as a discrete-event simulation drawing onto nets.
//! - [`net`]: a net's level over time, kept as edges.
//! - [`wire`]: UART/I²C/SPI waveforms, and a software UART receiver.
//! - [`fault`]: the faults on offer.
//! - [`bench`]: the world on a real-time clock, the virtual hub, the wiring.
//! - [`instruments`]: the logic analyzers and the scope as AWIC instruments.
//!
//! Every virtual device sits on bus [`SIM_BUS`] with vendor [`SIM_VID`], so
//! nothing virtual can be mistaken for real hardware.

pub mod bench;
pub mod fault;
pub mod instruments;
pub mod net;
mod rng;
pub mod wire;
pub mod world;

pub use bench::{PortError, SimBench, SimChange, SimSettings, SIM_BUS, SIM_VID};
pub use fault::Fault;
pub use instruments::SimBackend;
pub use wire::Parity;
pub use world::UartRead;
