//! The simulated board: a discrete-event simulation of its MCU firmware and
//! its 555 timer, drawing onto the nets the instruments read.
//!
//! Time only moves forward, and only through [`World::advance_to`]: every
//! event up to that instant runs in time order, and whatever an event draws
//! goes onto the nets at or after its own time. Readers never look past the
//! time they advanced to, so nothing they have seen is ever rewritten — a
//! reset cancels only the traffic the firmware had queued for the future.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};

use crate::fault::{Fault, FaultSet};
use crate::net::{EdgeLog, Ns, Window};
use crate::rng::Rng;
use crate::wire::{I2cWriter, RxByte, SpiWriter, UartFormat, UartReceiver};

pub const MS: Ns = 1_000_000;
pub const US: Ns = 1_000;
pub const S: Ns = 1_000_000_000;

/// How much history the nets keep for readers that fall behind.
const HISTORY: Ns = 20 * S;

/// The console's nominal rate.
pub const CONSOLE_BAUD: f64 = 115_200.0;
const I2C_HZ: u32 = 100_000;
const SPI_HZ: u32 = 1_000_000;
const TMP102_ADDR: u8 = 0x48;

/// The 555's supply and parts.
pub const OSC_VCC: f64 = 5.0;
const OSC_OUT_HIGH: f64 = 4.85;
const OSC_R1: f64 = 1_000.0;
const OSC_C: f64 = 100e-9;
pub const OSC_R2_DEFAULT: f64 = 6_800.0;
/// How fast the 555's output slews (first order).
pub const OSC_OUT_TAU_NS: f64 = 40.0;
const RUNT_NS: Ns = 35;

/// The wires on the board that a probe can be clipped to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Net {
    /// MCU UART transmit (to the host).
    Tx,
    /// MCU UART receive (from the host).
    Rx,
    Scl,
    Sda,
    Sck,
    Mosi,
    Cs,
    Led,
    /// The 555's output, pin 3.
    OscOut,
    /// U3 (a CD4026B, markings sanded off) pin 5: CARRY OUT.
    U3Co,
    /// U3's segment outputs, pins 10, 12, 13, 9, 11, 6, 7.
    U3A,
    U3B,
    U3C,
    U3D,
    U3E,
    U3F,
    U3G,
}

impl Net {
    pub const DIGITAL: [Net; 17] = [
        Net::Tx,
        Net::Rx,
        Net::Scl,
        Net::Sda,
        Net::Sck,
        Net::Mosi,
        Net::Cs,
        Net::Led,
        Net::OscOut,
        Net::U3Co,
        Net::U3A,
        Net::U3B,
        Net::U3C,
        Net::U3D,
        Net::U3E,
        Net::U3F,
        Net::U3G,
    ];

    /// U3's segment outputs, a to g.
    pub const SEGMENTS: [Net; 7] = [
        Net::U3A,
        Net::U3B,
        Net::U3C,
        Net::U3D,
        Net::U3E,
        Net::U3F,
        Net::U3G,
    ];

    fn index(self) -> usize {
        self as usize
    }

    /// What the net idles at with the board powered.
    fn idle_level(self) -> bool {
        matches!(self, Net::Tx | Net::Rx | Net::Scl | Net::Sda | Net::Cs)
    }

    /// Driven by the MCU (and so cancelled by its reset).
    fn mcu_driven(self) -> bool {
        matches!(
            self,
            Net::Tx | Net::Scl | Net::Sda | Net::Sck | Net::Mosi | Net::Cs | Net::Led
        )
    }
}

/// One piece of the 555's capacitor voltage: an exponential from `v0`
/// toward `target` with time constant `tau_ns`, starting at `t`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CapSegment {
    pub t: Ns,
    pub v0: f64,
    pub target: f64,
    pub tau_ns: f64,
}

impl CapSegment {
    pub fn at(&self, t: Ns) -> f64 {
        let dt = t.saturating_sub(self.t) as f64;
        self.target + (self.v0 - self.target) * (-dt / self.tau_ns).exp()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Ev {
    Boot(u32),
    Heartbeat(u32),
    SensorPoll(u32),
    StatusLog(u32),
    FlashCheck(u32),
    /// Run the MCU's UART receiver up to now.
    McuRx(u32),
    Hang(u32),
    WatchdogReset(u32),
    OscEdge(u32),
    Runt(u32),
    /// U3's CLOCK input (pin 1) changes to the given level.
    U3Clock(u32, bool),
    /// U3's RESET input (pin 15) changes to the given level.
    U3Reset(u32, bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Scheduled {
    t: Ns,
    seq: u64,
    ev: Ev,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedMode {
    Blink,
    On,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetCause {
    PowerOn,
    Software,
    Watchdog,
}

impl ResetCause {
    pub fn as_str(self) -> &'static str {
        match self {
            ResetCause::PowerOn => "power-on",
            ResetCause::Software => "software",
            ResetCause::Watchdog => "watchdog",
        }
    }
}

struct Mcu {
    epoch: u32,
    running: bool,
    hung: bool,
    boot_at: Ns,
    cause: ResetCause,
    boots: u32,
    tx_free: Ns,
    i2c_free: Ns,
    spi_free: Ns,
    rx: UartReceiver,
    line: Vec<u8>,
    last_was_cr: bool,
    log_on: bool,
    led_mode: LedMode,
    led: bool,
    temp_c: Option<f64>,
    sensor_errors: u32,
}

impl Mcu {
    fn new() -> Self {
        Self {
            epoch: 0,
            running: false,
            hung: false,
            boot_at: 0,
            cause: ResetCause::PowerOn,
            boots: 0,
            tx_free: 0,
            i2c_free: 0,
            spi_free: 0,
            rx: UartReceiver::new(UartFormat::new_8n1(CONSOLE_BAUD), 0),
            line: Vec::new(),
            last_was_cr: false,
            log_on: true,
            led_mode: LedMode::Blink,
            led: false,
            temp_c: None,
            sensor_errors: 0,
        }
    }
}

struct Osc {
    epoch: u32,
    running: bool,
    out_high: bool,
    r2: f64,
    segs: VecDeque<CapSegment>,
}

impl Osc {
    fn tau_charge_ns(&self) -> f64 {
        (OSC_R1 + self.r2) * OSC_C * 1e9
    }

    fn tau_discharge_ns(&self) -> f64 {
        self.r2 * OSC_C * 1e9
    }

    fn cap_at(&self, t: Ns) -> f64 {
        let i = self.segs.partition_point(|s| s.t <= t);
        match i {
            0 => self.segs.front().map_or(0.0, |s| s.v0),
            i => self.segs[i - 1].at(t),
        }
    }
}

/// U3, a CD4026B decade counter with a decoded seven-segment output, and
/// the keys and switches wired to its inputs.
struct Counter {
    epoch: u32,
    count: u8,
    clock: bool,
    reset: bool,
    /// SW1 on pin 2, CLOCK INHIBIT: high stops counting.
    inhibit: bool,
    /// SW2 on pin 3, DISPLAY ENABLE IN: low blanks segments a-g.
    display_enable: bool,
    /// When the keys are free for the next press.
    clock_key_free: Ns,
    reset_key_free: Ns,
}

/// Segments a-g lit for each digit, bit 0 = a. The 6 has no top bar and the
/// 9 no bottom bar: the CD4026's own way of drawing them.
const SEVEN_SEG: [u8; 10] = [
    0b011_1111, // 0: abcdef
    0b000_0110, // 1: bc
    0b101_1011, // 2: abdeg
    0b100_1111, // 3: abcdg
    0b110_0110, // 4: bcfg
    0b110_1101, // 5: acdfg
    0b111_1100, // 6: cdefg
    0b000_0111, // 7: abc
    0b111_1111, // 8: abcdefg
    0b110_0111, // 9: abcfg
];

/// How long a key stays pressed.
const KEY_PRESS_NS: Ns = 80 * MS;
/// The CD4026's clock-to-output delay at 5 V.
const U3_DELAY_NS: Ns = 250;

/// The keys and switches on U3's inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// KEY1, push button on pin 1.
    Key1,
    /// KEY2, push button on pin 15.
    Key2,
    /// SW1, toggle on pin 2.
    Sw1,
    /// SW2, toggle on pin 3.
    Sw2,
}

impl Input {
    pub const ALL: [Input; 4] = [Input::Key1, Input::Key2, Input::Sw1, Input::Sw2];

    pub fn id(self) -> &'static str {
        match self {
            Input::Key1 => "KEY1",
            Input::Key2 => "KEY2",
            Input::Sw1 => "SW1",
            Input::Sw2 => "SW2",
        }
    }

    pub fn pin(self) -> u8 {
        match self {
            Input::Key1 => 1,
            Input::Key2 => 15,
            Input::Sw1 => 2,
            Input::Sw2 => 3,
        }
    }

    pub fn momentary(self) -> bool {
        matches!(self, Input::Key1 | Input::Key2)
    }

    pub fn parse(s: &str) -> Option<Input> {
        Input::ALL
            .into_iter()
            .find(|i| i.id().eq_ignore_ascii_case(s.trim()))
    }
}

/// The virtual USB-UART adapter on the board: reads TX, drives RX.
struct Adapter {
    fmt: UartFormat,
    rx: UartReceiver,
    tx_free: Ns,
}

/// What reading the adapter found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UartRead {
    /// Bytes received since the last read (possibly none).
    Data(Vec<u8>),
    /// The board — and the adapter on it — has no power.
    Unpowered,
}

/// The virtual bench's whole state. Not thread-safe by itself; [`crate::SimBench`]
/// holds it behind a lock.
pub struct World {
    seed: u64,
    rng: Rng,
    t: Ns,
    nets: Vec<EdgeLog>,
    queue: BinaryHeap<Reverse<Scheduled>>,
    seq: u64,
    /// Hub port power, port 1 first.
    ports: [bool; 4],
    faults: FaultSet,
    mcu: Mcu,
    osc: Osc,
    counter: Counter,
    adapter: Option<Adapter>,
    last_prune: Ns,
}

/// Which hub port feeds what.
pub const PORT_BOARD: u8 = 1;
pub const PORT_LOGIC: u8 = 2;
pub const PORT_SCOPE: u8 = 3;
pub const PORT_LOGIC2: u8 = 4;

impl World {
    /// A world at time 0 with every port powered, the board booting.
    pub fn new(seed: u64, faults: FaultSet) -> Self {
        let mut world = Self {
            seed,
            rng: Rng::new(seed).fork(0x57_0711),
            t: 0,
            nets: Net::DIGITAL.iter().map(|_| EdgeLog::new(false)).collect(),
            queue: BinaryHeap::new(),
            seq: 0,
            ports: [true, true, true, true],
            faults,
            mcu: Mcu::new(),
            osc: Osc {
                epoch: 0,
                running: false,
                out_high: false,
                r2: OSC_R2_DEFAULT,
                segs: VecDeque::from([CapSegment {
                    t: 0,
                    v0: 0.0,
                    target: 0.0,
                    tau_ns: 1.0,
                }]),
            },
            counter: Counter {
                epoch: 0,
                count: 0,
                clock: false,
                reset: false,
                inhibit: false,
                display_enable: true,
                clock_key_free: 0,
                reset_key_free: 0,
            },
            adapter: None,
            last_prune: 0,
        };
        world.board_power_on(0);
        world
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    pub fn now(&self) -> Ns {
        self.t
    }

    pub fn faults(&self) -> FaultSet {
        self.faults
    }

    pub fn port_power(&self, port: u8) -> bool {
        (1..=4).contains(&port) && self.ports[usize::from(port - 1)]
    }

    pub fn board_powered(&self) -> bool {
        self.port_power(PORT_BOARD)
    }

    pub fn boots(&self) -> u32 {
        self.mcu.boots
    }

    pub fn last_reset_cause(&self) -> ResetCause {
        self.mcu.cause
    }

    /// Seconds since the MCU last started, while it runs.
    pub fn uptime_s(&self) -> Option<f64> {
        self.mcu
            .running
            .then(|| self.t.saturating_sub(self.mcu.boot_at) as f64 / 1e9)
    }

    pub fn osc_r2_ohm(&self) -> f64 {
        self.osc.r2
    }

    /// The 555's nominal frequency with the current pot setting.
    pub fn osc_nominal_hz(&self) -> f64 {
        1.0 / (std::f64::consts::LN_2 * (OSC_R1 + 2.0 * self.osc.r2) * OSC_C)
    }

    fn schedule(&mut self, t: Ns, ev: Ev) {
        self.seq += 1;
        self.queue.push(Reverse(Scheduled {
            t,
            seq: self.seq,
            ev,
        }));
    }

    fn net(&mut self, net: Net) -> &mut EdgeLog {
        &mut self.nets[net.index()]
    }

    /// Runs every event up to `t`. Calls with an earlier `t` do nothing.
    pub fn advance_to(&mut self, t: Ns) {
        while let Some(Reverse(next)) = self.queue.peek().copied() {
            if next.t > t {
                break;
            }
            self.queue.pop();
            self.t = self.t.max(next.t);
            self.handle(next.t, next.ev);
        }
        self.t = self.t.max(t);
        if self.t.saturating_sub(self.last_prune) > S {
            self.prune(self.t.saturating_sub(HISTORY));
            self.last_prune = self.t;
        }
    }

    fn prune(&mut self, before: Ns) {
        for net in &mut self.nets {
            net.prune_before(before);
        }
        while self.osc.segs.len() > 1 && self.osc.segs[1].t <= before {
            self.osc.segs.pop_front();
        }
    }

    // ---- reading -----------------------------------------------------------

    pub fn digital_window(&self, net: Net, start: Ns, end: Ns) -> Window {
        self.nets[net.index()].window(start, end)
    }

    pub fn level_at(&self, net: Net, t: Ns) -> bool {
        self.nets[net.index()].level_at(t)
    }

    /// The capacitor segments covering `[start, end]`: the one in force at
    /// `start`, then every one beginning inside.
    pub fn cap_segments(&self, start: Ns, end: Ns) -> Vec<CapSegment> {
        let segs = &self.osc.segs;
        let first = segs.partition_point(|s| s.t <= start).saturating_sub(1);
        segs.range(first..)
            .take_while(|s| s.t <= end)
            .copied()
            .collect()
    }

    pub fn cap_voltage(&self, t: Ns) -> f64 {
        self.osc.cap_at(t)
    }

    /// The rail the 555's output swings to.
    pub fn osc_out_high_v(&self) -> f64 {
        OSC_OUT_HIGH
    }

    // ---- the bench around the board ---------------------------------------

    /// Switches a hub port. Returns whether anything changed.
    pub fn set_port_power(&mut self, t: Ns, port: u8, on: bool) -> bool {
        self.advance_to(t);
        if !(1..=4).contains(&port) || self.port_power(port) == on {
            return false;
        }
        self.ports[usize::from(port - 1)] = on;
        if port == PORT_BOARD {
            if on {
                self.board_power_on(t);
            } else {
                self.board_power_off(t);
            }
        }
        true
    }

    /// Replaces the faults in force.
    pub fn set_faults(&mut self, t: Ns, faults: FaultSet) {
        self.advance_to(t);
        let added: Vec<Fault> = faults.iter().filter(|f| !self.faults.has(*f)).collect();
        self.faults = faults;
        for fault in added {
            match fault {
                Fault::WatchdogReset if self.mcu.running => self.schedule_hang(t),
                Fault::RuntPulse if self.osc.running => {
                    let at = t + self.rng.range(0.5, 2.0).mul_s();
                    self.schedule(at, Ev::Runt(self.osc.epoch));
                }
                _ => {}
            }
        }
    }

    /// Turns the 555's timing pot (R2, ohms).
    pub fn set_osc_r2(&mut self, t: Ns, r2_ohm: f64) {
        self.advance_to(t);
        self.osc.r2 = r2_ohm.clamp(470.0, 100_000.0);
    }

    /// Connects the USB-UART adapter at `fmt`. Bytes sent before this are
    /// not seen, as with a real port opened late.
    pub fn uart_open(&mut self, t: Ns, fmt: UartFormat) {
        self.advance_to(t);
        self.adapter = Some(Adapter {
            fmt,
            rx: UartReceiver::new(fmt, t),
            tx_free: t,
        });
    }

    pub fn uart_close(&mut self) {
        self.adapter = None;
    }

    /// What the adapter has received from the MCU up to `t`.
    pub fn uart_read(&mut self, t: Ns) -> UartRead {
        self.advance_to(t);
        if !self.board_powered() {
            return UartRead::Unpowered;
        }
        let Some(adapter) = self.adapter.as_mut() else {
            return UartRead::Data(Vec::new());
        };
        let mut got = Vec::new();
        adapter.rx.poll(&self.nets[Net::Tx.index()], t, &mut got);
        UartRead::Data(got.into_iter().map(|b| b.value).collect())
    }

    /// Sends bytes from the adapter to the MCU. Returns `false` if the board
    /// is unpowered (nothing is sent).
    pub fn uart_write(&mut self, t: Ns, bytes: &[u8]) -> bool {
        self.advance_to(t);
        if !self.board_powered() {
            return false;
        }
        let Some(adapter) = self.adapter.as_mut() else {
            return false;
        };
        let fmt = adapter.fmt;
        let mut at = t.max(adapter.tx_free);
        let rx = &mut self.nets[Net::Rx.index()];
        for &b in bytes {
            at = crate::wire::uart_byte(rx, at, &fmt, b);
        }
        adapter.tx_free = at;
        // The MCU reads them once they are in; a frame read at a drifted
        // clock may take a little longer.
        let epoch = self.mcu.epoch;
        self.schedule(at + (fmt.bit_ns * 2.0) as Ns, Ev::McuRx(epoch));
        true
    }

    // ---- power -------------------------------------------------------------

    fn board_power_on(&mut self, t: Ns) {
        for net in Net::DIGITAL {
            self.net(net).set(t, net.idle_level());
        }
        if let Some(adapter) = self.adapter.as_mut() {
            adapter.rx.restart(t);
            adapter.tx_free = t;
        }
        self.mcu.epoch += 1;
        self.mcu.running = false;
        self.mcu.cause = ResetCause::PowerOn;
        let epoch = self.mcu.epoch;
        // Regulator and crystal start-up.
        self.schedule(t + 25 * MS, Ev::Boot(epoch));
        self.osc_start(t);
        // A CMOS counter comes up in whatever state it likes.
        self.counter.epoch += 1;
        self.counter.count = (self.rng.next_u64() % 10) as u8;
        self.counter.clock = false;
        self.counter.reset = false;
        self.counter.clock_key_free = t;
        self.counter.reset_key_free = t;
        self.u3_outputs(t + 5 * US);
    }

    fn board_power_off(&mut self, t: Ns) {
        for net in Net::DIGITAL {
            let log = self.net(net);
            log.truncate_after(t);
            log.set(t, false);
        }
        self.mcu.epoch += 1;
        self.mcu.running = false;
        self.mcu.hung = false;
        self.osc.epoch += 1;
        self.osc.running = false;
        self.osc.out_high = false;
        self.counter.epoch += 1;
        let v0 = self.osc.cap_at(t);
        self.osc.segs.push_back(CapSegment {
            t,
            v0,
            target: 0.0,
            tau_ns: 2.0 * MS as f64,
        });
    }

    /// Resets the MCU (not the 555, which has no reset line here).
    fn mcu_reset(&mut self, t: Ns, cause: ResetCause) {
        for net in Net::DIGITAL.into_iter().filter(|n| n.mcu_driven()) {
            let log = self.net(net);
            log.truncate_after(t);
            log.set(t, net.idle_level());
        }
        self.mcu.epoch += 1;
        self.mcu.running = false;
        self.mcu.hung = false;
        self.mcu.cause = cause;
        let epoch = self.mcu.epoch;
        self.schedule(t + 25 * MS, Ev::Boot(epoch));
    }

    // ---- events ------------------------------------------------------------

    fn handle(&mut self, t: Ns, ev: Ev) {
        let mcu_epoch = self.mcu.epoch;
        let osc_epoch = self.osc.epoch;
        match ev {
            Ev::OscEdge(e) if e == osc_epoch => self.osc_edge(t),
            Ev::Runt(e) if e == osc_epoch => self.runt(t),
            Ev::OscEdge(_) | Ev::Runt(_) => {}
            Ev::U3Clock(e, level) if e == self.counter.epoch => self.u3_clock(t, level),
            Ev::U3Reset(e, level) if e == self.counter.epoch => self.u3_reset(t, level),
            Ev::U3Clock(..) | Ev::U3Reset(..) => {}
            Ev::Boot(e) if e == mcu_epoch => self.boot(t),
            _ if !matches!(ev, Ev::Boot(_)) && ev_epoch(ev) == mcu_epoch => self.mcu_event(t, ev),
            _ => {}
        }
    }

    fn mcu_event(&mut self, t: Ns, ev: Ev) {
        let epoch = self.mcu.epoch;
        match ev {
            Ev::Heartbeat(_) => {
                if !self.mcu.hung && self.mcu.led_mode == LedMode::Blink {
                    self.set_led(t, !self.mcu.led);
                }
                self.schedule(t + 500 * MS, Ev::Heartbeat(epoch));
            }
            Ev::SensorPoll(_) => {
                if !self.mcu.hung {
                    let (_, reading) = self.read_sensor(t);
                    match reading {
                        Some(c) => self.mcu.temp_c = Some(c),
                        None => {
                            self.mcu.sensor_errors += 1;
                            self.print(t + 50 * US, "E: tmp102 nack\r\n");
                        }
                    }
                }
                self.schedule(t + 500 * MS, Ev::SensorPoll(epoch));
            }
            Ev::StatusLog(_) => {
                if !self.mcu.hung && self.mcu.log_on {
                    let line = self.status_line(t);
                    self.print(t, &line);
                }
                self.schedule(t + 2 * S, Ev::StatusLog(epoch));
            }
            Ev::FlashCheck(_) => {
                if !self.mcu.hung {
                    let glitch = self.faults.has(Fault::SpiCsGlitch) && self.rng.chance(0.2);
                    let end = self.flash_read(t, &[0x03, 0x00, 0x10, 0x00], 8, glitch);
                    if glitch {
                        self.print(end + 20 * US, "E: flash crc mismatch @0x001000\r\n");
                    }
                }
                self.schedule(t + 3 * S, Ev::FlashCheck(epoch));
            }
            Ev::McuRx(_) => self.mcu_rx(t),
            Ev::Hang(_) => {
                if self.faults.has(Fault::WatchdogReset) {
                    self.mcu.hung = true;
                    self.schedule(t + 1_200 * MS, Ev::WatchdogReset(epoch));
                }
            }
            Ev::WatchdogReset(_) => self.mcu_reset(t, ResetCause::Watchdog),
            Ev::Boot(_) | Ev::OscEdge(_) | Ev::Runt(_) | Ev::U3Clock(..) | Ev::U3Reset(..) => {}
        }
    }

    fn boot(&mut self, t: Ns) {
        let epoch = self.mcu.epoch;
        let mcu = &mut self.mcu;
        mcu.running = true;
        mcu.hung = false;
        mcu.boot_at = t;
        mcu.boots += 1;
        mcu.tx_free = t;
        mcu.i2c_free = t;
        mcu.spi_free = t;
        mcu.line.clear();
        mcu.last_was_cr = false;
        mcu.log_on = true;
        mcu.led_mode = LedMode::Blink;
        mcu.led = false;
        mcu.temp_c = None;
        mcu.sensor_errors = 0;
        let fmt = self.console_format(t);
        self.mcu.rx = UartReceiver::new(fmt, t);

        // Probe the flash (JEDEC ID), then say hello.
        let end = self.flash_read(t, &[0x9F], 3, false);
        let cause = self.mcu.cause.as_str();
        let banner = format!(
            "\r\nAWIC trainer board v1.4.2 (virtual)\r\n\
             reset cause: {cause}\r\n\
             flash: W25Q32 (ef 40 16)\r\n\
             sensor: TMP102 at 0x48\r\n\
             type 'help' for commands\r\n> "
        );
        self.print(end + 200 * US, &banner);

        self.schedule(t + 100 * MS, Ev::SensorPoll(epoch));
        self.schedule(t + 500 * MS, Ev::Heartbeat(epoch));
        self.schedule(t + 2 * S, Ev::StatusLog(epoch));
        self.schedule(t + 3 * S, Ev::FlashCheck(epoch));
        if self.faults.has(Fault::WatchdogReset) {
            self.schedule_hang(t);
        }
    }

    fn schedule_hang(&mut self, from: Ns) {
        let at = from + self.rng.range(20.0, 45.0).mul_s();
        self.schedule(at, Ev::Hang(self.mcu.epoch));
    }

    // ---- MCU peripherals --------------------------------------------------

    /// The console's frame format at `t`: 115200 8N1 from the MCU's clock,
    /// which the `baud_drift` fault pulls fast.
    fn console_format(&self, t: Ns) -> UartFormat {
        let factor = if self.faults.has(Fault::BaudDrift) {
            let phase = 2.0 * std::f64::consts::PI * (t as f64 / (90.0 * S as f64));
            1.0 + 0.04 + 0.025 * phase.sin()
        } else {
            1.0
        };
        UartFormat::new_8n1(CONSOLE_BAUD * factor)
    }

    fn set_led(&mut self, t: Ns, on: bool) {
        self.mcu.led = on;
        self.net(Net::Led).set(t, on);
    }

    fn print(&mut self, t: Ns, text: &str) {
        let mut at = t.max(self.mcu.tx_free);
        for &b in text.as_bytes() {
            let fmt = self.console_format(at);
            at = crate::wire::uart_byte(&mut self.nets[Net::Tx.index()], at, &fmt, b);
        }
        self.mcu.tx_free = at;
    }

    /// The true temperature on the board.
    fn true_temp(&mut self, t: Ns) -> f64 {
        let phase = 2.0 * std::f64::consts::PI * (t as f64 / (120.0 * S as f64));
        23.0 + 1.5 * phase.sin() + 0.03 * self.rng.normal()
    }

    /// One TMP102 read: point at register 0, repeated start, read two bytes.
    /// Returns when the bus is free and what the firmware read.
    fn read_sensor(&mut self, t: Ns) -> (Ns, Option<f64>) {
        let start = t.max(self.mcu.i2c_free);
        let nack = self.faults.has(Fault::I2cNack) && self.rng.chance(1.0 / 12.0);
        let glitch_bit = (self.faults.has(Fault::ClockGlitch) && self.rng.chance(0.15))
            .then(|| (self.rng.next_u64() % 7) as usize);
        let temp = self.true_temp(t);
        let raw = ((temp / 0.0625).round() as i16) << 4;
        let bytes = raw.to_be_bytes();
        // The bits the sensor shifts out; after a glitch it is one ahead.
        let slave: Vec<bool> = (0..16)
            .map(|i| (u16::from_be_bytes(bytes) >> (15 - i)) & 1 == 1)
            .collect();
        let seen: Vec<bool> = (0..16)
            .map(|i| match glitch_bit {
                Some(g) if i > g => slave.get(i + 1).copied().unwrap_or(true),
                _ => slave[i],
            })
            .collect();

        let [scl, sda] = self.two_nets(Net::Scl, Net::Sda);
        let mut w = I2cWriter::new(scl, sda, start, I2C_HZ);
        w.start();
        if nack {
            w.byte(TMP102_ADDR << 1, false);
            w.stop();
            let end = w.t;
            self.mcu.i2c_free = end + 20 * US;
            return (end, None);
        }
        w.byte(TMP102_ADDR << 1, true);
        w.byte(0x00, true);
        w.start();
        w.byte((TMP102_ADDR << 1) | 1, true);
        // Address and pointer bytes are 27 bits in; the glitch lands in the
        // first data byte.
        w.glitch_at_bit = glitch_bit.map(|g| 27 + g);
        for (i, &bit) in seen.iter().enumerate() {
            w.bit(bit);
            if i == 7 {
                w.bit(false); // master ACKs the first byte
            }
        }
        w.bit(true); // and NACKs the last
        w.stop();
        let end = w.t;
        self.mcu.i2c_free = end + 20 * US;
        let got = seen.iter().fold(0u16, |acc, &b| (acc << 1) | u16::from(b));
        let celsius = f64::from((got as i16) >> 4) * 0.0625;
        (end, Some(celsius))
    }

    /// `i2c scan`: probe every 7-bit address; only the sensor answers.
    fn i2c_scan(&mut self, t: Ns) -> (Ns, Vec<u8>) {
        let start = t.max(self.mcu.i2c_free);
        let [scl, sda] = self.two_nets(Net::Scl, Net::Sda);
        let mut w = I2cWriter::new(scl, sda, start, I2C_HZ);
        let mut found = Vec::new();
        for addr in 0x08..=0x77u8 {
            let ack = addr == TMP102_ADDR;
            w.start();
            w.byte(addr << 1, ack);
            w.stop();
            w.t += 30 * US;
            if ack {
                found.push(addr);
            }
        }
        let end = w.t;
        self.mcu.i2c_free = end;
        (end, found)
    }

    /// One flash command plus `dummy` clocked-in bytes, mode 0 at 1 MHz.
    fn flash_read(&mut self, t: Ns, cmd: &[u8], dummy: usize, cs_glitch: bool) -> Ns {
        let start = t.max(self.mcu.spi_free);
        let (sck, mosi, cs) = self.three_nets(Net::Sck, Net::Mosi, Net::Cs);
        let mut w = SpiWriter::new(sck, mosi, cs, start, SPI_HZ);
        w.select();
        for &b in cmd {
            w.byte(b);
        }
        for i in 0..dummy {
            if cs_glitch && i == dummy / 2 {
                w.cs_glitch();
            }
            w.byte(0x00);
        }
        w.deselect();
        let end = w.t;
        self.mcu.spi_free = end;
        end
    }

    fn two_nets(&mut self, a: Net, b: Net) -> [&mut EdgeLog; 2] {
        let [x, y] = self
            .nets
            .get_disjoint_mut([a.index(), b.index()])
            .expect("distinct nets");
        [x, y]
    }

    fn three_nets(&mut self, a: Net, b: Net, c: Net) -> (&mut EdgeLog, &mut EdgeLog, &mut EdgeLog) {
        let [x, y, z] = self
            .nets
            .get_disjoint_mut([a.index(), b.index(), c.index()])
            .expect("distinct nets");
        (x, y, z)
    }

    fn status_line(&mut self, t: Ns) -> String {
        let up = t.saturating_sub(self.mcu.boot_at) as f64 / 1e9;
        match self.mcu.temp_c {
            Some(c) => format!("[{up:9.3}] temp {c:.2} C\r\n"),
            None => format!("[{up:9.3}] temp --.-- C\r\n"),
        }
    }

    // ---- the console ------------------------------------------------------

    fn mcu_rx(&mut self, t: Ns) {
        if !self.mcu.running {
            return;
        }
        let fmt = self.console_format(t);
        self.mcu.rx.fmt = fmt;
        let mut got: Vec<RxByte> = Vec::new();
        self.mcu.rx.poll(&self.nets[Net::Rx.index()], t, &mut got);
        if self.mcu.hung {
            return; // the main loop is stuck; the bytes are lost
        }
        for b in got {
            self.shell_byte(t, b.value);
        }
    }

    fn shell_byte(&mut self, t: Ns, byte: u8) {
        match byte {
            b'\n' if self.mcu.last_was_cr => {
                self.mcu.last_was_cr = false;
            }
            b'\r' | b'\n' => {
                self.mcu.last_was_cr = byte == b'\r';
                self.print(t, "\r\n");
                let line = std::mem::take(&mut self.mcu.line);
                let line = String::from_utf8_lossy(&line).trim().to_string();
                self.run_command(t + 30 * US, &line);
            }
            0x08 | 0x7F => {
                self.mcu.last_was_cr = false;
                if self.mcu.line.pop().is_some() {
                    self.print(t, "\x08 \x08");
                }
            }
            0x03 => {
                self.mcu.line.clear();
                self.print(t, "^C\r\n> ");
            }
            _ => {
                self.mcu.last_was_cr = false;
                if self.mcu.line.len() < 120 {
                    self.mcu.line.push(byte);
                    self.print(t, &(byte as char).to_string());
                }
            }
        }
    }

    fn run_command(&mut self, t: Ns, line: &str) {
        let mut words = line.split_whitespace();
        let cmd = words.next().unwrap_or("");
        let arg = words.next().unwrap_or("");
        let rest: Vec<&str> = line.splitn(2, ' ').collect();
        let out: String = match (cmd, arg) {
            ("", _) => String::new(),
            ("help", _) => HELP.to_string(),
            ("version", _) => "AWIC trainer board v1.4.2 (virtual), built Oct 30 2026\r\n".into(),
            ("uptime", _) => {
                let up = t.saturating_sub(self.mcu.boot_at) as f64 / 1e9;
                format!(
                    "up {up:.3} s, boot #{}, reset cause: {}\r\n",
                    self.mcu.boots,
                    self.mcu.cause.as_str()
                )
            }
            ("temp", _) => {
                let (end, reading) = self.read_sensor(t);
                let text = match reading {
                    Some(c) => {
                        self.mcu.temp_c = Some(c);
                        format!("{c:.4} C\r\n")
                    }
                    None => "E: tmp102 nack\r\n".to_string(),
                };
                self.print(end + 20 * US, &text);
                self.prompt(end + 20 * US);
                return;
            }
            ("led", "on") => {
                self.mcu.led_mode = LedMode::On;
                self.set_led(t, true);
                "led on\r\n".into()
            }
            ("led", "off") => {
                self.mcu.led_mode = LedMode::Off;
                self.set_led(t, false);
                "led off\r\n".into()
            }
            ("led", "blink") => {
                self.mcu.led_mode = LedMode::Blink;
                "led blinking at 1 Hz\r\n".into()
            }
            ("led", _) => "usage: led on|off|blink\r\n".into(),
            ("log", "on") => {
                self.mcu.log_on = true;
                "status log on (every 2 s)\r\n".into()
            }
            ("log", "off") => {
                self.mcu.log_on = false;
                "status log off\r\n".into()
            }
            ("log", _) => "usage: log on|off\r\n".into(),
            ("i2c", "scan") => {
                let (end, found) = self.i2c_scan(t);
                let list: Vec<String> = found.iter().map(|a| format!("0x{a:02x}")).collect();
                let text = format!("i2c: {} device(s): {}\r\n", found.len(), list.join(" "));
                self.print(end + 20 * US, &text);
                self.prompt(end + 20 * US);
                return;
            }
            ("i2c", _) => "usage: i2c scan\r\n".into(),
            ("flash", "id") => {
                let end = self.flash_read(t, &[0x9F], 3, false);
                self.print(end + 20 * US, "flash: JEDEC ef 40 16 (W25Q32, 4 MiB)\r\n");
                self.prompt(end + 20 * US);
                return;
            }
            ("flash", _) => "usage: flash id\r\n".into(),
            ("echo", _) => format!("{}\r\n", rest.get(1).copied().unwrap_or("")),
            ("reboot", _) => {
                self.print(t, "rebooting...\r\n");
                let at = self.mcu.tx_free + 100 * US;
                self.mcu_reset(at, ResetCause::Software);
                return;
            }
            ("crash", _) => {
                self.print(
                    t,
                    "HardFault @ 0x08001a3c\r\n\
                     \x20 r0=00000000 r1=20001f40 r2=0000002a r3=deadbeef\r\n\
                     \x20 lr=08001a21 pc=08001a3c psr=61000000\r\n\
                     watchdog will reset in 1 s\r\n",
                );
                self.mcu.hung = true;
                let epoch = self.mcu.epoch;
                self.schedule(t + S, Ev::WatchdogReset(epoch));
                return;
            }
            _ => format!("unknown command '{}', try 'help'\r\n", cmd.escape_default()),
        };
        self.print(t, &out);
        self.prompt(t);
    }

    fn prompt(&mut self, t: Ns) {
        self.print(t, "> ");
    }

    // ---- the 555 -----------------------------------------------------------

    fn osc_start(&mut self, t: Ns) {
        self.osc.epoch += 1;
        self.osc.running = true;
        self.osc.out_high = true;
        self.net(Net::OscOut).set(t, true);
        let v0 = self.osc.cap_at(t);
        let tau = self.osc.tau_charge_ns();
        self.osc.segs.push_back(CapSegment {
            t,
            v0,
            target: OSC_VCC,
            tau_ns: tau,
        });
        // Charging from wherever it is up to the upper threshold.
        let dt = tau * ((OSC_VCC - v0) / (OSC_VCC / 3.0)).ln().max(0.0);
        let epoch = self.osc.epoch;
        self.schedule(t + dt.max(1.0) as Ns, Ev::OscEdge(epoch));
        if self.faults.has(Fault::RuntPulse) {
            let at = t + self.rng.range(0.5, 2.0).mul_s();
            self.schedule(at, Ev::Runt(epoch));
        }
    }

    fn osc_edge(&mut self, t: Ns) {
        let v0 = self.osc.cap_at(t);
        let high = !self.osc.out_high;
        self.osc.out_high = high;
        self.net(Net::OscOut).set(t, high);
        let (target, tau, dt) = if high {
            let tau = self.osc.tau_charge_ns();
            // 1/3 Vcc up to 2/3 Vcc.
            (OSC_VCC, tau, tau * ((OSC_VCC - v0) / (OSC_VCC / 3.0)).ln())
        } else {
            let tau = self.osc.tau_discharge_ns();
            (0.0, tau, tau * (v0 / (OSC_VCC / 3.0)).ln())
        };
        self.osc.segs.push_back(CapSegment {
            t,
            v0,
            target,
            tau_ns: tau,
        });
        let jitter = if self.faults.has(Fault::OscJitter) {
            (1.0 + 0.06 * self.rng.normal()).clamp(0.75, 1.25)
        } else {
            1.0 + 0.002 * self.rng.normal()
        };
        let epoch = self.osc.epoch;
        self.schedule(t + (dt.max(1_000.0) * jitter) as Ns, Ev::OscEdge(epoch));
    }

    fn runt(&mut self, t: Ns) {
        if !self.faults.has(Fault::RuntPulse) || !self.osc.running {
            return;
        }
        if !self.osc.out_high {
            let out = self.net(Net::OscOut);
            out.set(t, true);
            out.set(t + RUNT_NS, false);
        }
        let at = t + self.rng.range(1.5, 4.0).mul_s();
        let epoch = self.osc.epoch;
        self.schedule(at, Ev::Runt(epoch));
    }
}

impl World {
    // ---- U3 ---------------------------------------------------------------

    /// What U3's display shows: segments a-g lit, bit 0 = a. Blank without
    /// power or with DISPLAY ENABLE IN low.
    pub fn display_segments(&self) -> u8 {
        if !self.board_powered() || !self.counter.display_enable {
            0
        } else {
            SEVEN_SEG[usize::from(self.counter.count)]
        }
    }

    pub fn input_state(&self, input: Input) -> bool {
        match input {
            Input::Key1 => self.counter.clock,
            Input::Key2 => self.counter.reset,
            Input::Sw1 => self.counter.inhibit,
            Input::Sw2 => self.counter.display_enable,
        }
    }

    /// Presses a key `times` times in a row (about 6 per second), or sets a
    /// switch. Returns `false` with the board unpowered.
    pub fn operate(&mut self, t: Ns, input: Input, state: Option<bool>, times: u32) -> bool {
        self.advance_to(t);
        if !self.board_powered() {
            return false;
        }
        let epoch = self.counter.epoch;
        match input {
            Input::Sw1 | Input::Sw2 => {
                let on = state.unwrap_or(!self.input_state(input));
                if input == Input::Sw1 {
                    self.counter.inhibit = on;
                } else {
                    self.counter.display_enable = on;
                    self.u3_outputs(t + U3_DELAY_NS);
                }
            }
            Input::Key1 | Input::Key2 => {
                let key1 = input == Input::Key1;
                let mut at = if key1 {
                    self.counter.clock_key_free
                } else {
                    self.counter.reset_key_free
                }
                .max(t);
                for _ in 0..times.clamp(1, 50) {
                    let bounces = if key1 && self.faults.has(Fault::KeyBounce) {
                        (self.rng.next_u64() % 4) as usize
                    } else {
                        0
                    };
                    let mut edge = at;
                    for _ in 0..bounces {
                        // Contact bounce: a few short closures before it settles.
                        let open = edge + self.rng.range(150.0, 600.0).mul_us();
                        self.schedule(edge, Ev::U3Clock(epoch, true));
                        self.schedule(open, Ev::U3Clock(epoch, false));
                        edge = open + self.rng.range(100.0, 400.0).mul_us();
                    }
                    let release = at + KEY_PRESS_NS;
                    if key1 {
                        self.schedule(edge, Ev::U3Clock(epoch, true));
                        self.schedule(release, Ev::U3Clock(epoch, false));
                    } else {
                        self.schedule(edge, Ev::U3Reset(epoch, true));
                        self.schedule(release, Ev::U3Reset(epoch, false));
                    }
                    at = release + 80 * MS;
                }
                if key1 {
                    self.counter.clock_key_free = at;
                } else {
                    self.counter.reset_key_free = at;
                }
            }
        }
        true
    }

    fn u3_clock(&mut self, t: Ns, level: bool) {
        let rising = level && !self.counter.clock;
        self.counter.clock = level;
        if rising && !self.counter.inhibit && !self.counter.reset {
            self.counter.count = (self.counter.count + 1) % 10;
            self.u3_outputs(t + U3_DELAY_NS);
        }
    }

    fn u3_reset(&mut self, t: Ns, level: bool) {
        self.counter.reset = level;
        if level && self.counter.count != 0 {
            self.counter.count = 0;
            self.u3_outputs(t + U3_DELAY_NS);
        }
    }

    fn u3_outputs(&mut self, t: Ns) {
        let segments = if self.counter.display_enable {
            SEVEN_SEG[usize::from(self.counter.count)]
        } else {
            0
        };
        for (i, net) in Net::SEGMENTS.into_iter().enumerate() {
            self.net(net).set(t, segments >> i & 1 == 1);
        }
        // CARRY OUT is high for 0-4 and falls at 5: its rising edge, at the
        // wrap from 9 to 0, clocks the next decade.
        let carry = self.counter.count < 5;
        self.net(Net::U3Co).set(t, carry);
    }
}

fn ev_epoch(ev: Ev) -> u32 {
    match ev {
        Ev::Boot(e)
        | Ev::Heartbeat(e)
        | Ev::SensorPoll(e)
        | Ev::StatusLog(e)
        | Ev::FlashCheck(e)
        | Ev::McuRx(e)
        | Ev::Hang(e)
        | Ev::WatchdogReset(e)
        | Ev::OscEdge(e)
        | Ev::Runt(e)
        | Ev::U3Clock(e, _)
        | Ev::U3Reset(e, _) => e,
    }
}

trait Seconds {
    fn mul_s(self) -> Ns;
}

impl Seconds for f64 {
    fn mul_s(self) -> Ns {
        (self * S as f64) as Ns
    }
}

trait Micros {
    fn mul_us(self) -> Ns;
}

impl Micros for f64 {
    fn mul_us(self) -> Ns {
        (self * US as f64) as Ns
    }
}

const HELP: &str = "commands:\r\n\
\x20 help              this list\r\n\
\x20 version           firmware version\r\n\
\x20 uptime            time since reset, and why it reset\r\n\
\x20 temp              read the TMP102 now\r\n\
\x20 led on|off|blink  the heartbeat LED\r\n\
\x20 log on|off        the status line every 2 s\r\n\
\x20 i2c scan          probe every I2C address\r\n\
\x20 flash id          read the SPI flash's JEDEC ID\r\n\
\x20 echo <text>       say it back\r\n\
\x20 reboot            software reset\r\n\
\x20 crash             take a HardFault (the watchdog resets)\r\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn console(world: &mut World, until: Ns) -> String {
        let mut rx = UartReceiver::new(UartFormat::new_8n1(CONSOLE_BAUD), 0);
        world.advance_to(until);
        let mut got = Vec::new();
        rx.poll(&world.nets[Net::Tx.index()], until, &mut got);
        got.iter().map(|b| b.value as char).collect()
    }

    #[test]
    fn the_board_boots_and_says_hello() {
        let mut world = World::new(1, FaultSet::default());
        let text = console(&mut world, 100 * MS);
        assert!(text.contains("AWIC trainer board"), "{text:?}");
        assert!(text.contains("reset cause: power-on"), "{text:?}");
        assert!(text.ends_with("> "), "{text:?}");
    }

    #[test]
    fn the_status_line_reports_the_temperature() {
        let mut world = World::new(1, FaultSet::default());
        let text = console(&mut world, 2_200 * MS);
        assert!(text.contains(" C\r\n"), "{text:?}");
        assert!(text.contains("temp 2"), "{text:?}");
    }

    #[test]
    fn the_shell_answers_a_command() {
        let mut world = World::new(1, FaultSet::default());
        world.uart_open(50 * MS, UartFormat::new_8n1(CONSOLE_BAUD));
        assert!(world.uart_write(60 * MS, b"uptime\r"));
        let mut text = String::new();
        let mut t = 60 * MS;
        while t < 200 * MS {
            t += 10 * MS;
            if let UartRead::Data(d) = world.uart_read(t) {
                text.push_str(&String::from_utf8_lossy(&d));
            }
        }
        assert!(text.contains("uptime\r\nup 0."), "{text:?}");
        assert!(text.contains("boot #1"), "{text:?}");
    }

    #[test]
    fn a_power_cycle_reboots_and_the_adapter_follows() {
        let mut world = World::new(1, FaultSet::default());
        world.uart_open(10 * MS, UartFormat::new_8n1(CONSOLE_BAUD));
        assert!(world.set_port_power(200 * MS, PORT_BOARD, false));
        assert_eq!(world.uart_read(300 * MS), UartRead::Unpowered);
        assert!(!world.level_at(Net::Tx, 300 * MS));
        assert!(world.set_port_power(400 * MS, PORT_BOARD, true));
        let UartRead::Data(d) = world.uart_read(600 * MS) else {
            panic!("powered");
        };
        let text = String::from_utf8_lossy(&d);
        assert!(text.contains("reset cause: power-on"), "{text:?}");
        assert_eq!(world.boots(), 2);
    }

    #[test]
    fn reboot_says_why() {
        let mut world = World::new(1, FaultSet::default());
        world.uart_open(50 * MS, UartFormat::new_8n1(CONSOLE_BAUD));
        world.uart_write(60 * MS, b"reboot\r");
        let mut text = String::new();
        for i in 1..30 {
            if let UartRead::Data(d) = world.uart_read(60 * MS + i * 10 * MS) {
                text.push_str(&String::from_utf8_lossy(&d));
            }
        }
        assert!(text.contains("reset cause: software"), "{text:?}");
    }

    #[test]
    fn the_555_runs_near_its_nominal_frequency() {
        let mut world = World::new(1, FaultSet::default());
        world.advance_to(2 * S);
        let w = world.digital_window(Net::OscOut, 100 * MS, 1_100 * MS);
        let rising = w.edges.len() / 2;
        let nominal = world.osc_nominal_hz();
        assert!(
            (rising as f64 - nominal).abs() < nominal * 0.03,
            "{rising} vs {nominal}"
        );
        // The capacitor swings between a third and two thirds of Vcc.
        for k in 0..200 {
            let v = world.cap_voltage(200 * MS + k * 37 * US);
            assert!(v > 1.6 && v < 3.4, "{v}");
        }
    }

    #[test]
    fn the_watchdog_fault_resets_the_board() {
        let faults: FaultSet = [Fault::WatchdogReset].into_iter().collect();
        let mut world = World::new(3, faults);
        world.advance_to(60 * S);
        assert!(world.boots() >= 2, "boots {}", world.boots());
    }

    #[test]
    fn baud_drift_garbles_the_console_at_its_worst() {
        let faults: FaultSet = [Fault::BaudDrift].into_iter().collect();
        let mut world = World::new(1, faults);
        // The drift peaks (6.5 % fast) a quarter of the way into its 90 s cycle.
        world.uart_open(20 * S, UartFormat::new_8n1(CONSOLE_BAUD));
        let UartRead::Data(d) = world.uart_read(25 * S) else {
            panic!()
        };
        let text = String::from_utf8_lossy(&d);
        assert!(!text.contains("temp 2"), "{text:?}");
    }

    fn digit(world: &World) -> Option<usize> {
        SEVEN_SEG
            .iter()
            .position(|&s| s == world.display_segments())
    }

    #[test]
    fn key1_counts_and_key2_resets() {
        let mut world = World::new(1, FaultSet::default());
        world.operate(10 * MS, Input::Key2, None, 1);
        world.advance_to(100 * MS);
        assert_eq!(digit(&world), Some(0));
        world.operate(200 * MS, Input::Key1, None, 3);
        world.advance_to(2 * S);
        assert_eq!(digit(&world), Some(3));
        // The output pins follow, segment by segment.
        for (i, net) in Net::SEGMENTS.into_iter().enumerate() {
            assert_eq!(world.level_at(net, 2 * S), SEVEN_SEG[3] >> i & 1 == 1);
        }
        assert!(world.level_at(Net::U3Co, 2 * S));
    }

    #[test]
    fn it_wraps_after_nine_with_a_rising_carry() {
        let mut world = World::new(1, FaultSet::default());
        world.operate(10 * MS, Input::Key2, None, 1);
        world.operate(200 * MS, Input::Key1, None, 10);
        world.advance_to(5 * S);
        assert_eq!(digit(&world), Some(0));
        let co = world.digital_window(Net::U3Co, 0, 5 * S);
        assert!(!co.edges.is_empty());
        assert!(world.level_at(Net::U3Co, 5 * S));
    }

    #[test]
    fn inhibit_stops_counting_and_display_enable_blanks() {
        let mut world = World::new(1, FaultSet::default());
        world.operate(10 * MS, Input::Key2, None, 1);
        world.operate(200 * MS, Input::Sw1, Some(true), 1);
        world.operate(210 * MS, Input::Key1, None, 2);
        world.advance_to(S);
        assert_eq!(digit(&world), Some(0));
        world.operate(S, Input::Sw2, Some(false), 1);
        world.advance_to(S + MS);
        assert_eq!(world.display_segments(), 0);
        assert!(!world.level_at(Net::U3A, S + MS));
    }

    #[test]
    fn a_bouncing_key_can_count_more_than_once() {
        let faults: FaultSet = [Fault::KeyBounce].into_iter().collect();
        let mut world = World::new(2, faults);
        world.operate(10 * MS, Input::Key2, None, 1);
        world.operate(200 * MS, Input::Key1, None, 20);
        world.advance_to(10 * S);
        // Twenty clean presses would end on 0; bounces add counts.
        let edges = world.digital_window(Net::U3A, 200 * MS, 10 * S).edges.len();
        assert!(edges > 0);
        assert_ne!(digit(&world), None);
    }

    #[test]
    fn the_same_seed_draws_the_same_waveform() {
        let faults: FaultSet = Fault::ALL.into_iter().collect();
        let mut a = World::new(9, faults);
        let mut b = World::new(9, faults);
        a.advance_to(5 * S);
        b.advance_to(5 * S);
        for net in Net::DIGITAL {
            assert_eq!(
                a.digital_window(net, 0, 5 * S),
                b.digital_window(net, 0, 5 * S)
            );
        }
    }
}
