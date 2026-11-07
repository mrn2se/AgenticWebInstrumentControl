//! [`SimBench`]: the world on a real-time clock, plus everything around the
//! board — the virtual hub, the probe wiring, the secrets — that clients see.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use awic_core::DeviceId;
use awic_proto::{
    SimBoard, SimCounter, SimFault, SimInput, SimInstrument, SimProbe, SimStatus, SimTestPoint,
    UsbPort, UsbTreeNode,
};
use tokio::sync::broadcast;
use tokio::time::Instant;

use crate::fault::{Fault, FaultSet};
use crate::net::Ns;
use crate::rng::Rng;
use crate::wire::{Parity, UartFormat};
use crate::world::{Input, Net, UartRead, World, PORT_BOARD, PORT_LOGIC, PORT_LOGIC2, PORT_SCOPE};

/// A bus number no host controller enumerates (the fake backend has 253).
pub const SIM_BUS: u8 = 254;
/// USB-IF's "no vendor": marks every virtual device as synthetic.
pub const SIM_VID: u16 = 0xFFFF;
pub const SIM_PID_ROOT: u16 = 0x5300;
pub const SIM_PID_HUB: u16 = 0x5301;
pub const SIM_PID_BOARD: u16 = 0x5302;
pub const SIM_PID_LOGIC: u16 = 0x5303;
pub const SIM_PID_SCOPE: u16 = 0x5304;
pub const SIM_PID_LOGIC2: u16 = 0x5305;

pub const BOARD_NAME: &str = "AWIC trainer board (virtual)";
pub const LOGIC_LABEL: &str = "AWIC virtual logic analyzer (8 channels)";
pub const SCOPE_LABEL: &str = "AWIC virtual oscilloscope (2 channels)";
pub const LOGIC2_LABEL: &str = "AWIC virtual logic analyzer #2 (8 channels)";

/// U3's eight output pins in pin order, as the second logic analyzer's
/// channels 0-7 are clipped to them: CARRY OUT, f, g, d, a, e, b, c.
pub const LOGIC2_WIRING: [Net; 8] = [
    Net::U3Co,
    Net::U3F,
    Net::U3G,
    Net::U3D,
    Net::U3A,
    Net::U3E,
    Net::U3B,
    Net::U3C,
];

/// `[sim]` as the bench needs it.
#[derive(Debug, Clone, Default)]
pub struct SimSettings {
    /// 0 picks one at random (and reports it, so a run can be repeated).
    pub seed: u64,
    /// Fault ids, or `"mystery"` for one secret random fault.
    pub faults: Vec<String>,
    /// Clip the logic analyzer's probes onto the test points in a random
    /// order, for the user to find out.
    pub shuffle: bool,
}

/// What a scope probe is clipped to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeProbe {
    /// TP10, the 555's timing capacitor.
    Cap,
    /// TP9, the 555's output.
    Out,
}

/// Which test point each instrument channel is clipped to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wiring {
    pub logic: [Net; 8],
    pub logic2: [Net; 8],
    pub scope: [ScopeProbe; 2],
}

/// A change clients should hear about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimChange {
    /// A virtual device appeared on or left the bus (`device` is its bus path).
    Usb { device: String, added: bool },
    /// Anything in [`SimStatus`].
    Status,
}

#[derive(Debug, Clone, Default)]
struct PortMeta {
    locked: Option<bool>,
    label: Option<String>,
    dut: bool,
}

struct Secrets {
    mystery: bool,
    revealed: bool,
    solved: bool,
    wrong_guesses: u32,
}

struct Inner {
    world: Mutex<World>,
    epoch: Instant,
    wiring: Wiring,
    shuffled: bool,
    secrets: Mutex<Secrets>,
    ports: Mutex<HashMap<u8, PortMeta>>,
    changes: broadcast::Sender<SimChange>,
}

/// The virtual bench. Cheap to clone.
#[derive(Clone)]
pub struct SimBench(Arc<Inner>);

/// The test points on the board, in order.
pub const TEST_POINTS: [(&str, Option<Net>, &str, &str); 18] = [
    (
        "TP1",
        Some(Net::Tx),
        "UART TX",
        "MCU console transmit, 3.3 V, idles high",
    ),
    (
        "TP2",
        Some(Net::Rx),
        "UART RX",
        "MCU console receive, driven by the on-board USB-UART",
    ),
    (
        "TP3",
        Some(Net::Scl),
        "I2C SCL",
        "Sensor bus clock, 100 kHz, 4.7 kΩ pull-up",
    ),
    (
        "TP4",
        Some(Net::Sda),
        "I2C SDA",
        "Sensor bus data; a TMP102 temperature sensor sits on it",
    ),
    (
        "TP5",
        Some(Net::Sck),
        "SPI SCK",
        "Flash clock, 1 MHz, mode 0",
    ),
    (
        "TP6",
        Some(Net::Mosi),
        "SPI MOSI",
        "Flash data in (MCU to flash)",
    ),
    (
        "TP7",
        Some(Net::Cs),
        "SPI CS#",
        "Flash chip select, active low",
    ),
    ("TP8", Some(Net::Led), "LED", "Heartbeat LED, high = on"),
    (
        "TP9",
        Some(Net::OscOut),
        "555 OUT",
        "555 astable output (pin 3), 5 V",
    ),
    (
        "TP10",
        None,
        "555 CAP",
        "555 timing capacitor (pins 2 and 6), swings between 1/3 and 2/3 of 5 V",
    ),
    ("U3.5", Some(Net::U3Co), "U3 pin 5", "U3 output"),
    ("U3.6", Some(Net::U3F), "U3 pin 6", "U3 output"),
    ("U3.7", Some(Net::U3G), "U3 pin 7", "U3 output"),
    ("U3.9", Some(Net::U3D), "U3 pin 9", "U3 output"),
    ("U3.10", Some(Net::U3A), "U3 pin 10", "U3 output"),
    ("U3.11", Some(Net::U3E), "U3 pin 11", "U3 output"),
    ("U3.12", Some(Net::U3B), "U3 pin 12", "U3 output"),
    ("U3.13", Some(Net::U3C), "U3 pin 13", "U3 output"),
];

/// What U3 really is.
pub const U3_PART: &str = "CD4026B";

/// The test points these nets are on, in board order — which says nothing
/// about which channel is on which.
fn probed_points(nets: &[Net]) -> Vec<String> {
    TEST_POINTS
        .iter()
        .filter(|tp| tp.1.is_some_and(|n| nets.contains(&n)))
        .map(|tp| tp.0.to_string())
        .collect()
}

fn test_point_of(net: Net) -> &'static str {
    TEST_POINTS
        .iter()
        .find(|tp| tp.1 == Some(net))
        .map(|tp| tp.0)
        .expect("every net has a test point")
}

impl SimBench {
    pub fn new(settings: &SimSettings) -> Result<Self, String> {
        let seed = if settings.seed == 0 {
            random_seed()
        } else {
            settings.seed
        };
        let rng = Rng::new(seed);

        let mut mystery = false;
        let mut faults = FaultSet::default();
        for id in &settings.faults {
            if id.trim() == "mystery" {
                mystery = true;
            } else {
                faults.insert(id.parse::<Fault>()?);
            }
        }
        if mystery {
            faults = FaultSet::default();
            faults.insert(random_fault(&mut rng.fork(0x0057_3733)));
        }

        let mut logic = [
            Net::Tx,
            Net::Rx,
            Net::Scl,
            Net::Sda,
            Net::Sck,
            Net::Mosi,
            Net::Cs,
            Net::Led,
        ];
        if settings.shuffle {
            rng.fork(0x0057_1e1d).shuffle(&mut logic);
        }
        let wiring = Wiring {
            logic,
            logic2: LOGIC2_WIRING,
            scope: [ScopeProbe::Cap, ScopeProbe::Out],
        };

        let mut ports = HashMap::new();
        ports.insert(
            PORT_BOARD,
            PortMeta {
                locked: None,
                label: Some("DUT: AWIC trainer board".to_string()),
                dut: true,
            },
        );
        let (changes, _) = broadcast::channel(64);
        Ok(Self(Arc::new(Inner {
            world: Mutex::new(World::new(seed, faults)),
            epoch: Instant::now(),
            wiring,
            shuffled: settings.shuffle,
            secrets: Mutex::new(Secrets {
                mystery,
                revealed: false,
                solved: false,
                wrong_guesses: 0,
            }),
            ports: Mutex::new(ports),
            changes,
        })))
    }

    /// Simulated time now: nanoseconds since the bench was built.
    pub fn now_ns(&self) -> Ns {
        self.0.epoch.elapsed().as_nanos() as Ns
    }

    pub fn seed(&self) -> u64 {
        self.world().seed()
    }

    pub fn wiring(&self) -> &Wiring {
        &self.0.wiring
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SimChange> {
        self.0.changes.subscribe()
    }

    fn world(&self) -> MutexGuard<'_, World> {
        self.0.world.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The world, advanced to `t` (which must not be in the future: callers
    /// pass a time they have waited for).
    pub fn read_at<R>(&self, t: Ns, f: impl FnOnce(&World) -> R) -> R {
        let mut world = self.world();
        world.advance_to(t);
        f(&world)
    }

    /// The world advanced to now, for changing it.
    fn with_world<R>(&self, f: impl FnOnce(&mut World, Ns) -> R) -> R {
        let mut world = self.world();
        let now = self.now_ns().max(world.now());
        world.advance_to(now);
        f(&mut world, now)
    }

    fn changed(&self, change: SimChange) {
        let _ = self.0.changes.send(change);
    }

    /// Keeps the simulation moving while nobody reads it, so history stays
    /// bounded and the board's clock-driven events (a watchdog reset, the
    /// next boot) are announced as they happen. Ends when the bench is dropped.
    pub fn spawn_ticker(&self) -> tokio::task::JoinHandle<()> {
        let weak = Arc::downgrade(&self.0);
        tokio::spawn(async move {
            let mut last_boots = 0;
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
            loop {
                interval.tick().await;
                let Some(inner) = weak.upgrade() else { break };
                let bench = SimBench(inner);
                let boots = bench.with_world(|w, _| w.boots());
                if boots != last_boots {
                    last_boots = boots;
                    bench.changed(SimChange::Status);
                }
            }
        })
    }

    // ---- devices -----------------------------------------------------------

    pub fn hub_id() -> DeviceId {
        DeviceId::new(SIM_BUS, vec![1], SIM_VID, SIM_PID_HUB)
    }

    pub fn board_id() -> DeviceId {
        DeviceId::new(SIM_BUS, vec![1, PORT_BOARD], SIM_VID, SIM_PID_BOARD)
    }

    pub fn logic_id() -> DeviceId {
        DeviceId::new(SIM_BUS, vec![1, PORT_LOGIC], SIM_VID, SIM_PID_LOGIC)
    }

    pub fn scope_id() -> DeviceId {
        DeviceId::new(SIM_BUS, vec![1, PORT_SCOPE], SIM_VID, SIM_PID_SCOPE)
    }

    pub fn logic2_id() -> DeviceId {
        DeviceId::new(SIM_BUS, vec![1, PORT_LOGIC2], SIM_VID, SIM_PID_LOGIC2)
    }

    /// The board's USB-UART as a terminal source id.
    pub fn serial_source() -> String {
        format!("serial:{}:1.0", Self::board_id())
    }

    pub fn is_sim_hub(hub: &str) -> bool {
        hub == Self::hub_id().to_string()
    }

    pub fn port_power(&self, port: u8) -> bool {
        self.with_world(|w, _| w.port_power(port))
    }

    /// The instruments currently on the bus.
    pub fn powered_instruments(&self) -> Vec<DeviceId> {
        self.with_world(|w, _| {
            let mut ids = Vec::new();
            if w.port_power(PORT_LOGIC) {
                ids.push(Self::logic_id());
            }
            if w.port_power(PORT_SCOPE) {
                ids.push(Self::scope_id());
            }
            if w.port_power(PORT_LOGIC2) {
                ids.push(Self::logic2_id());
            }
            ids
        })
    }

    fn device_on(port: u8) -> Option<DeviceId> {
        match port {
            PORT_BOARD => Some(Self::board_id()),
            PORT_LOGIC => Some(Self::logic_id()),
            PORT_SCOPE => Some(Self::scope_id()),
            PORT_LOGIC2 => Some(Self::logic2_id()),
            _ => None,
        }
    }

    /// Switches a virtual hub port. A locked port refuses.
    pub fn set_port_power(&self, port: u8, on: bool) -> Result<bool, PortError> {
        if !(1..=4).contains(&port) {
            return Err(PortError::NoSuchPort(port));
        }
        if self.port_locked(port) {
            return Err(PortError::Locked(port));
        }
        let changed = self.with_world(|w, now| w.set_port_power(now, port, on));
        if changed {
            if let Some(device) = Self::device_on(port) {
                self.changed(SimChange::Usb {
                    device: device.to_string(),
                    added: on,
                });
            }
            self.changed(SimChange::Status);
        }
        Ok(on)
    }

    fn port_locked(&self, port: u8) -> bool {
        self.0
            .ports
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&port)
            .and_then(|m| m.locked)
            .unwrap_or(false)
    }

    /// Locks (`Some(true)`) or unlocks a port. Nothing on the virtual hub is
    /// protected by default, so unlocking and "automatic" are the same.
    pub fn set_port_locked(&self, port: u8, locked: Option<bool>) -> Result<bool, PortError> {
        if !(1..=4).contains(&port) {
            return Err(PortError::NoSuchPort(port));
        }
        let mut ports = self.0.ports.lock().unwrap_or_else(|e| e.into_inner());
        ports.entry(port).or_default().locked = locked;
        Ok(locked == Some(true))
    }

    pub fn set_port_label(
        &self,
        port: u8,
        label: Option<String>,
        dut: Option<bool>,
    ) -> Result<(Option<String>, bool), PortError> {
        if !(1..=4).contains(&port) {
            return Err(PortError::NoSuchPort(port));
        }
        let mut ports = self.0.ports.lock().unwrap_or_else(|e| e.into_inner());
        let meta = ports.entry(port).or_default();
        meta.label = label.filter(|l| !l.trim().is_empty());
        if let Some(dut) = dut {
            meta.dut = dut;
        }
        Ok((meta.label.clone(), meta.dut))
    }

    /// The virtual bus as `usb.tree` shows it: a root hub with the virtual
    /// hub on its only port, and the board and instruments behind that.
    pub fn usb_tree(&self) -> UsbTreeNode {
        let power: Vec<bool> = self.with_world(|w, _| (1..=4).map(|p| w.port_power(p)).collect());
        let meta = self
            .0
            .ports
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let leaf = |id: DeviceId, product: &str, instrument: bool| UsbTreeNode {
            bus: id.bus,
            port_path: id.port_path.clone(),
            vid: id.vid,
            pid: id.pid,
            product: Some(product.to_string()),
            speed: Some("480".to_string()),
            is_hub: false,
            ppps: None,
            is_instrument: instrument,
            ports: Vec::new(),
        };
        let ports = (1..=4u8)
            .map(|number| {
                let on = power[usize::from(number - 1)];
                let m = meta.get(&number).cloned().unwrap_or_default();
                let locked = m.locked == Some(true);
                let protection = locked.then(|| "locked by you".to_string());
                let device = on.then(|| match number {
                    PORT_BOARD => Some(leaf(
                        Self::board_id(),
                        "AWIC trainer board (virtual) — USB-UART",
                        false,
                    )),
                    PORT_LOGIC => Some(leaf(Self::logic_id(), LOGIC_LABEL, true)),
                    PORT_SCOPE => Some(leaf(Self::scope_id(), SCOPE_LABEL, true)),
                    PORT_LOGIC2 => Some(leaf(Self::logic2_id(), LOGIC2_LABEL, true)),
                    _ => None,
                });
                UsbPort {
                    number,
                    power: Some(on),
                    power_switchable: true,
                    protected: locked,
                    non_switchable_reason: protection.clone(),
                    protection,
                    derived_protection: None,
                    user_choice: m.locked,
                    label: m.label,
                    dut: m.dut,
                    switchable: !locked,
                    device: device.flatten().map(Box::new),
                }
            })
            .collect();
        let hub_id = Self::hub_id();
        let hub = UsbTreeNode {
            bus: SIM_BUS,
            port_path: hub_id.port_path.clone(),
            vid: SIM_VID,
            pid: SIM_PID_HUB,
            product: Some("AWIC virtual hub (4 ports)".to_string()),
            speed: Some("480".to_string()),
            is_hub: true,
            ppps: Some(true),
            is_instrument: false,
            ports,
        };
        UsbTreeNode {
            bus: SIM_BUS,
            port_path: Vec::new(),
            vid: SIM_VID,
            pid: SIM_PID_ROOT,
            product: Some("AWIC virtual bench".to_string()),
            speed: Some("480".to_string()),
            is_hub: true,
            ppps: Some(false),
            is_instrument: false,
            ports: vec![UsbPort {
                number: 1,
                power: Some(true),
                power_switchable: false,
                protected: true,
                protection: Some("a hub is attached".to_string()),
                derived_protection: Some("a hub is attached".to_string()),
                user_choice: None,
                label: None,
                dut: false,
                switchable: false,
                non_switchable_reason: Some(
                    "this hub does not support per-port power switching".to_string(),
                ),
                device: Some(Box::new(hub)),
            }],
        }
    }

    // ---- the board's USB-UART ---------------------------------------------

    /// Opens the board's USB-UART at the given frame format.
    pub fn uart_open(&self, baud: u32, data_bits: u8, parity: Parity, stop_bits: u8) {
        let fmt = UartFormat {
            bit_ns: 1e9 / f64::from(baud.max(1)),
            data_bits: data_bits.clamp(5, 8),
            parity,
            stop_bits: stop_bits.clamp(1, 2),
        };
        self.with_world(|w, now| w.uart_open(now, fmt));
    }

    pub fn uart_close(&self) {
        self.with_world(|w, _| w.uart_close());
    }

    pub fn uart_read(&self) -> UartRead {
        self.with_world(|w, now| w.uart_read(now))
    }

    pub fn uart_write(&self, bytes: &[u8]) -> bool {
        self.with_world(|w, now| w.uart_write(now, bytes))
    }

    pub fn board_powered(&self) -> bool {
        self.port_power(PORT_BOARD)
    }

    // ---- faults and secrets ------------------------------------------------

    /// Puts exactly these faults in force, ending any mystery.
    pub fn set_faults(&self, ids: &[String]) -> Result<(), String> {
        let faults = ids
            .iter()
            .map(|id| id.parse::<Fault>())
            .collect::<Result<FaultSet, String>>()?;
        self.with_world(|w, now| w.set_faults(now, faults));
        let mut secrets = self.0.secrets.lock().unwrap_or_else(|e| e.into_inner());
        secrets.mystery = false;
        drop(secrets);
        self.changed(SimChange::Status);
        Ok(())
    }

    /// One random fault, kept secret.
    pub fn start_mystery(&self) {
        let pick = {
            let mut rng = Rng::new(self.now_ns() ^ self.seed());
            random_fault(&mut rng)
        };
        self.with_world(|w, now| w.set_faults(now, [pick].into_iter().collect()));
        let mut secrets = self.0.secrets.lock().unwrap_or_else(|e| e.into_inner());
        secrets.mystery = true;
        secrets.revealed = false;
        drop(secrets);
        self.changed(SimChange::Status);
    }

    pub fn reveal(&self) {
        self.0
            .secrets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .revealed = true;
        self.changed(SimChange::Status);
    }

    /// Presses one of U3's keys `presses` times, or sets (or toggles) a switch.
    pub fn operate(&self, input: &str, state: Option<bool>, presses: u32) -> Result<(), String> {
        let which = Input::parse(input).ok_or_else(|| {
            format!("unknown input {input:?}; the board has KEY1, KEY2, SW1 and SW2")
        })?;
        if which.momentary() && state == Some(false) {
            return Err(format!(
                "{} is a push button: press it, do not set it",
                which.id()
            ));
        }
        let ok = self.with_world(|w, now| w.operate(now, which, state, presses));
        if !ok {
            return Err("the board has no power (hub port 1 is off)".to_string());
        }
        self.changed(SimChange::Status);
        Ok(())
    }

    /// Checks a guess at U3's part number. Any maker's prefix and suffix
    /// count (CD4026B, HEF4026B, MC14026B, 4026).
    pub fn guess(&self, part: &str) -> bool {
        let digits: String = part.chars().filter(|c| c.is_ascii_digit()).collect();
        let right = digits == "4026" || digits == "14026";
        let mut secrets = self.0.secrets.lock().unwrap_or_else(|e| e.into_inner());
        if right {
            secrets.solved = true;
        } else {
            secrets.wrong_guesses += 1;
        }
        drop(secrets);
        self.changed(SimChange::Status);
        right
    }

    pub fn set_osc_r2(&self, ohm: f64) -> Result<(), String> {
        if !ohm.is_finite() || !(470.0..=100_000.0).contains(&ohm) {
            return Err(format!(
                "osc_r2_ohm must be 470..100000 (the pot's range), got {ohm}"
            ));
        }
        self.with_world(|w, now| w.set_osc_r2(now, ohm));
        self.changed(SimChange::Status);
        Ok(())
    }

    pub fn status(&self) -> SimStatus {
        let (segments, inputs) = self.with_world(|w, _| {
            (
                w.display_segments(),
                Input::ALL.map(|i| (i, w.input_state(i))),
            )
        });
        let (seed, faults, powered, uptime, boots, cause, r2, hz, logic_on, scope_on, logic2_on) =
            self.with_world(|w, _| {
                (
                    w.seed(),
                    w.faults(),
                    w.board_powered(),
                    w.uptime_s(),
                    w.boots(),
                    w.last_reset_cause().as_str(),
                    w.osc_r2_ohm(),
                    w.osc_nominal_hz(),
                    w.port_power(PORT_LOGIC),
                    w.port_power(PORT_SCOPE),
                    w.port_power(PORT_LOGIC2),
                )
            });
        let (mystery, revealed, solved, wrong_guesses) = {
            let s = self.0.secrets.lock().unwrap_or_else(|e| e.into_inner());
            (s.mystery, s.revealed, s.solved, s.wrong_guesses)
        };
        let show_wiring = revealed || !self.0.shuffled;
        let show_faults = revealed || !mystery;
        let wiring = &self.0.wiring;
        let logic_probes = wiring
            .logic
            .iter()
            .enumerate()
            .map(|(ch, &net)| SimProbe {
                channel: ch as u8,
                test_point: show_wiring.then(|| test_point_of(net).to_string()),
            })
            .collect();
        let scope_probes = wiring
            .scope
            .iter()
            .enumerate()
            .map(|(ch, probe)| SimProbe {
                channel: ch as u8,
                test_point: Some(
                    match probe {
                        ScopeProbe::Cap => "TP10",
                        ScopeProbe::Out => "TP9",
                    }
                    .to_string(),
                ),
            })
            .collect();
        let logic2_probes = wiring
            .logic2
            .iter()
            .enumerate()
            .map(|(ch, &net)| SimProbe {
                channel: ch as u8,
                test_point: Some(test_point_of(net).to_string()),
            })
            .collect();
        let display: String = (0..7u8)
            .filter(|i| segments >> i & 1 == 1)
            .map(|i| char::from(b'a' + i))
            .collect();
        let input_text = |i: Input| match i {
            Input::Key1 => "Push button to U3 pin 1, pulled low, high while pressed",
            Input::Key2 => "Push button to U3 pin 15, pulled low, high while pressed",
            Input::Sw1 => "Toggle switch to U3 pin 2 (on = high)",
            Input::Sw2 => "Toggle switch to U3 pin 3 (on = high)",
        };
        SimStatus {
            seed,
            counter: SimCounter {
                name: "U3".to_string(),
                description: "A 16-pin CMOS DIP with its markings sanded off, on 5 V (pin 16) \
                    and ground (pin 8), driving the common-cathode seven-segment display DS1. \
                    Two push buttons and two switches reach four of its inputs; logic \
                    analyzer #2 is clipped to eight of its outputs. Which part is it?"
                    .to_string(),
                part: (solved || revealed).then(|| U3_PART.to_string()),
                solved,
                wrong_guesses,
                inputs: inputs
                    .iter()
                    .map(|&(i, state)| SimInput {
                        id: i.id().to_string(),
                        pin: i.pin(),
                        momentary: i.momentary(),
                        state,
                        description: input_text(i).to_string(),
                    })
                    .collect(),
                display,
            },
            hub: Self::hub_id().to_string(),
            board: SimBoard {
                name: BOARD_NAME.to_string(),
                description: "An MCU with a serial console, a TMP102 temperature sensor on \
                    I2C, a W25Q32 flash on SPI and a heartbeat LED; a 555 timer running \
                    astable at about 1 kHz; and U3, an unmarked chip with keys and a \
                    seven-segment display. Only the test points are reachable."
                    .to_string(),
                device: Self::board_id().to_string(),
                port: PORT_BOARD,
                powered,
                uptime_s: uptime,
                boots,
                last_reset_cause: cause.to_string(),
                serial_source: Self::serial_source(),
                console: "115200 8N1".to_string(),
                test_points: TEST_POINTS
                    .iter()
                    .map(|(id, net, name, description)| SimTestPoint {
                        id: (*id).to_string(),
                        name: (*name).to_string(),
                        description: (*description).to_string(),
                        kind: if net.is_some() { "digital" } else { "analog" }.to_string(),
                    })
                    .collect(),
            },
            instruments: vec![
                SimInstrument {
                    device: Self::logic_id().to_string(),
                    label: LOGIC_LABEL.to_string(),
                    port: PORT_LOGIC,
                    powered: logic_on,
                    probes: logic_probes,
                    probed_points: probed_points(&wiring.logic),
                },
                SimInstrument {
                    device: Self::scope_id().to_string(),
                    label: SCOPE_LABEL.to_string(),
                    port: PORT_SCOPE,
                    powered: scope_on,
                    probes: scope_probes,
                    probed_points: vec!["TP9".to_string(), "TP10".to_string()],
                },
                SimInstrument {
                    device: Self::logic2_id().to_string(),
                    label: LOGIC2_LABEL.to_string(),
                    port: PORT_LOGIC2,
                    powered: logic2_on,
                    probes: logic2_probes,
                    probed_points: probed_points(&wiring.logic2),
                },
            ],
            faults: Fault::ALL
                .iter()
                .map(|&f| SimFault {
                    id: f.id().to_string(),
                    title: f.title().to_string(),
                    symptom: f.symptom().to_string(),
                    active: show_faults.then(|| faults.has(f)),
                    cause: revealed.then(|| f.cause().to_string()),
                })
                .collect(),
            mystery,
            revealed,
            osc_r2_ohm: r2,
            osc_nominal_hz: hz,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortError {
    NoSuchPort(u8),
    Locked(u8),
}

impl std::fmt::Display for PortError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PortError::NoSuchPort(p) => write!(f, "the virtual hub has ports 1-4, not {p}"),
            PortError::Locked(p) => write!(f, "port {p} is locked; unlock it first"),
        }
    }
}

fn random_fault(rng: &mut Rng) -> Fault {
    Fault::ALL[(rng.next_u64() % Fault::ALL.len() as u64) as usize]
}

/// A seed from the clock, small enough to read out and type back in.
fn random_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1);
    (crate::rng::splitmix64(nanos) % 900_000) + 100_000
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bench(shuffle: bool, faults: &[&str]) -> SimBench {
        SimBench::new(&SimSettings {
            seed: 5,
            faults: faults.iter().map(|s| s.to_string()).collect(),
            shuffle,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn a_shuffled_wiring_is_hidden_until_revealed() {
        let b = bench(true, &[]);
        let status = b.status();
        assert!(status.instruments[0]
            .probes
            .iter()
            .all(|p| p.test_point.is_none()));
        // Where the probes are is no secret, only which is which.
        assert_eq!(
            status.instruments[0].probed_points,
            ["TP1", "TP2", "TP3", "TP4", "TP5", "TP6", "TP7", "TP8"]
        );
        b.reveal();
        let status = b.status();
        assert!(status.instruments[0]
            .probes
            .iter()
            .all(|p| p.test_point.is_some()));
    }

    #[tokio::test]
    async fn a_mystery_hides_which_fault_is_on() {
        let b = bench(false, &["mystery"]);
        let status = b.status();
        assert!(status.mystery);
        assert!(status.faults.iter().all(|f| f.active.is_none()));
        b.reveal();
        let status = b.status();
        assert_eq!(
            status
                .faults
                .iter()
                .filter(|f| f.active == Some(true))
                .count(),
            1
        );
        assert!(status.faults.iter().all(|f| f.cause.is_some()));
    }

    #[tokio::test]
    async fn an_unknown_fault_is_refused_by_name() {
        let err = SimBench::new(&SimSettings {
            faults: vec!["gremlins".into()],
            ..Default::default()
        })
        .err()
        .unwrap();
        assert!(err.contains("gremlins"));
    }

    #[tokio::test]
    async fn switching_a_port_announces_the_device() {
        let b = bench(false, &[]);
        let mut changes = b.subscribe();
        b.set_port_power(PORT_LOGIC, false).unwrap();
        assert_eq!(
            changes.recv().await.unwrap(),
            SimChange::Usb {
                device: "254-1.2".into(),
                added: false
            }
        );
        assert_eq!(
            b.powered_instruments(),
            vec![SimBench::scope_id(), SimBench::logic2_id()]
        );
        let tree = b.usb_tree();
        let hub = tree.ports[0].device.as_ref().unwrap();
        assert!(hub.ports[1].device.is_none());
        assert_eq!(hub.ports[1].power, Some(false));
    }

    #[tokio::test]
    async fn guessing_the_counter_reveals_it() {
        let b = bench(false, &[]);
        assert!(b.status().counter.part.is_none());
        assert!(!b.guess("74LS90"));
        assert!(b.guess("cd4026"));
        let status = b.status();
        assert_eq!(status.counter.part.as_deref(), Some("CD4026B"));
        assert_eq!(status.counter.wrong_guesses, 1);
    }

    #[tokio::test]
    async fn keys_drive_the_display() {
        let b = bench(false, &[]);
        b.operate("KEY2", None, 1).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(b.status().counter.display, "abcdef");
        assert!(b.operate("KEY9", None, 1).unwrap_err().contains("KEY1"));
    }

    #[tokio::test]
    async fn a_locked_port_refuses() {
        let b = bench(false, &[]);
        b.set_port_locked(1, Some(true)).unwrap();
        assert_eq!(b.set_port_power(1, false), Err(PortError::Locked(1)));
    }
}
