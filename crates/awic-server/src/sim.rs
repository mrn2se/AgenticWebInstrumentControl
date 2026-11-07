//! The virtual bench (`[sim] enabled`) wired into the server: its board's
//! USB-UART as a `serial` terminal source, its hub inside `usb.tree`, its
//! changes on the event bus, and `sim.status`/`sim.set`.
//!
//! Everything else — the instruments — goes through the ordinary
//! `Backend`/`Instrument` path (`awic_sim::SimBackend`), so the graph,
//! recorder, decoders, MCP and SCPI treat them like hardware.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use awic_core::EventBus;
use awic_proto::{
    Event, Parity, SimStatus, StopBits, TerminalKind, TerminalSourceInfo, UsbChange,
    UsbLabelResult, UsbPowerResult, UsbProtectResult, UsbTreeResult, PROTOCOL_VERSION,
};
use awic_sim::{PortError, SimBench, SimChange, UartRead};

use crate::error::UsbOpError;
use crate::terminal::{
    Connected, OpenSettings, SourceConnection, SourceEvent, TerminalError, TerminalSourceProvider,
};
use crate::usb::UsbController;

/// How often an open console session looks for bytes from the board.
const UART_POLL: Duration = Duration::from_millis(20);

// ---- the board's USB-UART ----------------------------------------------------

/// The trainer board's USB-UART, listed beside the real serial ports while
/// the board has power — as a real board's would be.
pub struct SimSerialProvider {
    bench: SimBench,
}

impl SimSerialProvider {
    pub fn new(bench: SimBench) -> Self {
        Self { bench }
    }
}

#[async_trait]
impl TerminalSourceProvider for SimSerialProvider {
    fn kind(&self) -> TerminalKind {
        TerminalKind::Serial
    }

    fn list(&self) -> Vec<TerminalSourceInfo> {
        if !self.bench.board_powered() {
            return Vec::new();
        }
        vec![TerminalSourceInfo {
            source: SimBench::serial_source(),
            kind: TerminalKind::Serial,
            label: "ttySIM0 — AWIC trainer board (virtual), console 115200 8N1".to_string(),
            device: Some(SimBench::board_id().to_string()),
            available: true,
            unavailable_reason: None,
        }]
    }

    async fn connect(
        &self,
        source_id: &str,
        settings: OpenSettings,
    ) -> Result<Connected, TerminalError> {
        if source_id != SimBench::serial_source() {
            return Err(TerminalError::UnknownSource(source_id.to_string()));
        }
        let serial = settings
            .serial
            .ok_or_else(|| TerminalError::SettingsRequired(source_id.to_string()))?;
        if !self.bench.board_powered() {
            return Err(TerminalError::Unavailable(
                source_id.to_string(),
                "the board has no power (virtual hub port 1 is off)".to_string(),
            ));
        }
        let parity = match serial.parity {
            Parity::None => awic_sim::Parity::None,
            Parity::Even => awic_sim::Parity::Even,
            Parity::Odd => awic_sim::Parity::Odd,
        };
        let stop_bits = match serial.stop_bits {
            StopBits::One => 1,
            StopBits::Two => 2,
        };
        self.bench
            .uart_open(serial.baud, serial.data_bits, parity, stop_bits);
        Ok(Connected {
            connection: Box::new(SimSerialConnection {
                bench: self.bench.clone(),
                waiting: false,
                pending: None,
            }),
            initial_backlog: Vec::new(),
            label: "ttySIM0 — AWIC trainer board (virtual)".to_string(),
        })
    }
}

struct SimSerialConnection {
    bench: SimBench,
    waiting: bool,
    /// Bytes read in the same poll that found the board back, delivered
    /// right after `Resumed`.
    pending: Option<Vec<u8>>,
}

#[async_trait]
impl SourceConnection for SimSerialConnection {
    async fn recv(&mut self) -> SourceEvent {
        if let Some(data) = self.pending.take() {
            return SourceEvent::Data(data);
        }
        loop {
            tokio::time::sleep(UART_POLL).await;
            match self.bench.uart_read() {
                UartRead::Unpowered if !self.waiting => {
                    self.waiting = true;
                    return SourceEvent::Waiting(
                        "the board lost power (virtual hub port 1)".to_string(),
                    );
                }
                UartRead::Unpowered => {}
                UartRead::Data(data) if self.waiting => {
                    self.waiting = false;
                    if !data.is_empty() {
                        self.pending = Some(data);
                    }
                    return SourceEvent::Resumed;
                }
                UartRead::Data(data) if !data.is_empty() => return SourceEvent::Data(data),
                UartRead::Data(_) => {}
            }
        }
    }

    async fn write(&mut self, data: &[u8]) -> Result<(), TerminalError> {
        if self.bench.uart_write(data) {
            Ok(())
        } else {
            Err(TerminalError::Io(
                "the board has no power (virtual hub port 1 is off)".to_string(),
            ))
        }
    }
}

impl Drop for SimSerialConnection {
    fn drop(&mut self) {
        self.bench.uart_close();
    }
}

// ---- the virtual hub ---------------------------------------------------------

/// The real USB controller with the virtual hub added: its subtree is
/// appended to `usb.tree`, and operations on it go to the bench.
pub struct SimUsbController {
    inner: Arc<dyn UsbController>,
    bench: SimBench,
}

impl SimUsbController {
    pub fn new(inner: Arc<dyn UsbController>, bench: SimBench) -> Self {
        Self { inner, bench }
    }
}

fn port_error(err: PortError) -> UsbOpError {
    let code = match err {
        PortError::NoSuchPort(_) => "invalid_params",
        PortError::Locked(_) => "port_protected",
    };
    UsbOpError {
        code: code.to_string(),
        msg: err.to_string(),
    }
}

#[async_trait]
impl UsbController for SimUsbController {
    async fn tree(&self) -> Result<UsbTreeResult, UsbOpError> {
        // A host without USB access still has the virtual hub to show.
        let mut tree = match self.inner.tree().await {
            Ok(tree) => tree,
            Err(err) => {
                tracing::debug!(code = %err.code, msg = %err.msg, "real USB tree unavailable");
                UsbTreeResult { roots: Vec::new() }
            }
        };
        tree.roots.push(self.bench.usb_tree());
        Ok(tree)
    }

    async fn power(&self, hub: &str, port: u8, on: bool) -> Result<UsbPowerResult, UsbOpError> {
        if !SimBench::is_sim_hub(hub) {
            return self.inner.power(hub, port, on).await;
        }
        let on = self.bench.set_port_power(port, on).map_err(port_error)?;
        Ok(UsbPowerResult {
            hub: hub.to_string(),
            port,
            on,
        })
    }

    async fn protect(
        &self,
        hub: &str,
        port: u8,
        protected: Option<bool>,
    ) -> Result<UsbProtectResult, UsbOpError> {
        if !SimBench::is_sim_hub(hub) {
            return self.inner.protect(hub, port, protected).await;
        }
        let locked = self
            .bench
            .set_port_locked(port, protected)
            .map_err(port_error)?;
        Ok(UsbProtectResult {
            hub: hub.to_string(),
            port,
            protected: locked,
            reason: locked.then(|| "locked by you".to_string()),
            user_choice: protected,
        })
    }

    async fn label(
        &self,
        hub: &str,
        port: u8,
        label: Option<String>,
        dut: Option<bool>,
    ) -> Result<UsbLabelResult, UsbOpError> {
        if !SimBench::is_sim_hub(hub) {
            return self.inner.label(hub, port, label, dut).await;
        }
        let (label, dut) = self
            .bench
            .set_port_label(port, label, dut)
            .map_err(port_error)?;
        Ok(UsbLabelResult {
            hub: hub.to_string(),
            port,
            label,
            dut,
        })
    }
}

// ---- events and requests -----------------------------------------------------

/// Puts the bench's changes on the event bus: a virtual device coming or
/// going as `usb.changed` (which also makes instruments follow, and the
/// terminal source list refresh), anything else as `sim.changed`.
pub async fn forward_changes(bench: SimBench, events: EventBus) {
    use tokio::sync::broadcast::error::RecvError;
    let mut changes = bench.subscribe();
    loop {
        match changes.recv().await {
            Ok(SimChange::Usb { device, added }) => events.publish(Event::UsbChanged {
                v: PROTOCOL_VERSION,
                device,
                change: if added {
                    UsbChange::Added
                } else {
                    UsbChange::Removed
                },
            }),
            Ok(SimChange::Status) | Err(RecvError::Lagged(_)) => {
                events.publish(Event::SimChanged {
                    v: PROTOCOL_VERSION,
                })
            }
            Err(RecvError::Closed) => break,
        }
    }
}

/// What `sim.set` may change, all optional.
#[derive(Debug, Default, Clone)]
pub struct SimChangeRequest {
    pub faults: Option<Vec<String>>,
    pub mystery: Option<bool>,
    pub reveal: Option<bool>,
    pub osc_r2_ohm: Option<f64>,
    pub input: Option<String>,
    pub state: Option<bool>,
    pub presses: Option<u32>,
    pub guess: Option<String>,
}

/// Applies a `sim.set`, all or nothing where it can be checked up front.
/// Returns the new status, and for a guess whether it was right.
pub fn apply(bench: &SimBench, req: SimChangeRequest) -> Result<(SimStatus, Option<bool>), String> {
    if let Some(ohm) = req.osc_r2_ohm {
        bench.set_osc_r2(ohm)?;
    }
    if let Some(faults) = &req.faults {
        bench.set_faults(faults)?;
    }
    if req.mystery == Some(true) {
        bench.start_mystery();
    }
    if let Some(input) = &req.input {
        bench.operate(input, req.state, req.presses.unwrap_or(1))?;
    }
    let verdict = req.guess.as_deref().map(|g| bench.guess(g));
    if req.reveal == Some(true) {
        bench.reveal();
    }
    Ok((bench.status(), verdict))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awic_sim::SimSettings;

    fn bench() -> SimBench {
        SimBench::new(&SimSettings {
            seed: 1,
            faults: vec![],
            shuffle: false,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn the_virtual_hub_is_added_to_a_failing_real_tree() {
        let usb = SimUsbController::new(Arc::new(crate::usb::UnimplementedUsbController), bench());
        let tree = usb.tree().await.unwrap();
        assert_eq!(tree.roots.len(), 1);
        assert_eq!(tree.roots[0].bus, awic_sim::SIM_BUS);
    }

    #[tokio::test]
    async fn power_on_the_virtual_hub_goes_to_the_bench_and_others_pass_through() {
        let b = bench();
        let usb =
            SimUsbController::new(Arc::new(crate::usb::UnimplementedUsbController), b.clone());
        let off = usb.power("254-1", 1, false).await.unwrap();
        assert!(!off.on);
        assert!(!b.board_powered());
        let err = usb.power("3-3", 1, false).await.unwrap_err();
        assert_eq!(err.code, "not_implemented");
    }

    #[tokio::test]
    async fn the_console_answers_through_the_terminal_source() {
        let b = bench();
        tokio::time::sleep(Duration::from_millis(60)).await;
        let provider = SimSerialProvider::new(b.clone());
        assert_eq!(provider.list().len(), 1);
        let settings = awic_proto::SerialSettings {
            baud: 115_200,
            data_bits: 8,
            parity: Parity::None,
            stop_bits: StopBits::One,
        };
        let mut conn = provider
            .connect(&SimBench::serial_source(), OpenSettings::serial(settings))
            .await
            .unwrap()
            .connection;
        conn.write(b"echo hi\r").await.unwrap();
        let mut text = String::new();
        while !text.contains("hi\r\n> ") {
            match tokio::time::timeout(Duration::from_secs(2), conn.recv()).await {
                Ok(SourceEvent::Data(d)) => text.push_str(&String::from_utf8_lossy(&d)),
                Ok(_) => {}
                Err(_) => panic!("no answer: {text:?}"),
            }
        }
    }

    #[tokio::test]
    async fn the_console_waits_while_the_board_is_off() {
        let b = bench();
        let provider = SimSerialProvider::new(b.clone());
        let settings = awic_proto::SerialSettings {
            baud: 115_200,
            data_bits: 8,
            parity: Parity::None,
            stop_bits: StopBits::One,
        };
        let mut conn = provider
            .connect(&SimBench::serial_source(), OpenSettings::serial(settings))
            .await
            .unwrap()
            .connection;
        b.set_port_power(1, false).unwrap();
        loop {
            if let SourceEvent::Waiting(_) = conn.recv().await {
                break;
            }
        }
        assert!(provider.list().is_empty());
        b.set_port_power(1, true).unwrap();
        loop {
            if let SourceEvent::Resumed = conn.recv().await {
                break;
            }
        }
    }

    #[tokio::test]
    async fn a_wrong_guess_counts_and_a_right_one_names_the_part() {
        let b = bench();
        let (_, verdict) = apply(
            &b,
            SimChangeRequest {
                guess: Some("NE555".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(verdict, Some(false));
        let (status, verdict) = apply(
            &b,
            SimChangeRequest {
                guess: Some("CD4026BE".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(verdict, Some(true));
        assert!(status.counter.solved);
    }
}
