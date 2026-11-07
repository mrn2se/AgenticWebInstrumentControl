//! Drawing bus traffic onto nets, and reading a UART back off one.
//!
//! The builders write real waveforms — start bits, clock edges, ACK slots —
//! so AWIC's own decoders, run on what the virtual logic analyzer captures,
//! read the same bytes the firmware meant to send. Faults are injected here,
//! at the waveform, never as a flag beside it.

use crate::net::{EdgeLog, Ns};

/// UART frame format, as a person writes it: `115200 8N1`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UartFormat {
    /// Bit time in nanoseconds (fractional: 115 200 baud is 8680.6 ns).
    pub bit_ns: f64,
    pub data_bits: u8,
    pub parity: Parity,
    pub stop_bits: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parity {
    None,
    Even,
    Odd,
}

impl UartFormat {
    pub fn new_8n1(baud: f64) -> Self {
        Self {
            bit_ns: 1e9 / baud,
            data_bits: 8,
            parity: Parity::None,
            stop_bits: 1,
        }
    }

    fn parity_bits(&self) -> u8 {
        u8::from(self.parity != Parity::None)
    }

    /// Start, data, parity and stop bits.
    pub fn frame_bits(&self) -> u8 {
        1 + self.data_bits + self.parity_bits() + self.stop_bits
    }

    pub fn frame_ns(&self) -> f64 {
        self.bit_ns * f64::from(self.frame_bits())
    }

    fn parity_of(&self, value: u16) -> bool {
        let ones = (value & ((1 << self.data_bits) - 1)).count_ones();
        match self.parity {
            Parity::None => false,
            Parity::Even => ones % 2 == 1,
            Parity::Odd => ones.is_multiple_of(2),
        }
    }
}

/// Writes one frame starting at `start`, returning when the line is free
/// again (the end of the last stop bit).
pub fn uart_byte(net: &mut EdgeLog, start: Ns, fmt: &UartFormat, byte: u8) -> Ns {
    let at = |bit: u8| start + (fmt.bit_ns * f64::from(bit)).round() as Ns;
    net.set(start, false);
    let mut bit = 1;
    for i in 0..fmt.data_bits {
        net.set(at(bit), (u16::from(byte) >> i) & 1 == 1);
        bit += 1;
    }
    if fmt.parity != Parity::None {
        net.set(at(bit), fmt.parity_of(u16::from(byte)));
        bit += 1;
    }
    net.set(at(bit), true);
    at(fmt.frame_bits())
}

/// A byte a [`UartReceiver`] read, with what a UART chip would flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RxByte {
    /// When its start bit began.
    pub at: Ns,
    pub value: u8,
    /// The stop bit read low.
    pub framing_error: bool,
    pub parity_error: bool,
}

/// A software UART receiver, as a USB-UART chip or an MCU's peripheral
/// works: wait for a falling edge, sample each bit at its middle, and look
/// for the next start bit only after sampling the stop bit. Read at the
/// wrong baud rate — or from a transmitter whose clock has drifted — it
/// produces the same garbage a real one would.
#[derive(Debug, Clone)]
pub struct UartReceiver {
    pub fmt: UartFormat,
    /// The next start bit is looked for strictly after this.
    cursor: Ns,
}

impl UartReceiver {
    /// Reads frames whose start bit begins at `from` or later.
    pub fn new(fmt: UartFormat, from: Ns) -> Self {
        Self {
            fmt,
            cursor: from.saturating_sub(1),
        }
    }

    /// Restarts reading at `t`, forgetting anything before it.
    pub fn restart(&mut self, t: Ns) {
        self.cursor = t.saturating_sub(1);
    }

    /// Reads every frame on `net` that is complete by `until`.
    pub fn poll(&mut self, net: &EdgeLog, until: Ns, out: &mut Vec<RxByte>) {
        let fmt = self.fmt;
        loop {
            // A start bit is a falling edge from idle-high; a line already
            // low (a break, or no power) starts nothing until it rises.
            let Some(start) = net.next_edge_to(self.cursor, false) else {
                // Nothing pending: skip ahead, but stay where a start bit
                // that began before `until` could still be found.
                return;
            };
            let sample = |bit: f64| start + (fmt.bit_ns * (bit + 0.5)).round() as Ns;
            let stop_at = sample(f64::from(fmt.frame_bits() - fmt.stop_bits));
            if stop_at > until {
                return;
            }
            // A glitch shorter than half a bit is not a start bit.
            if net.level_at(sample(0.0)) {
                self.cursor = sample(0.0);
                continue;
            }
            let mut value: u16 = 0;
            for i in 0..fmt.data_bits {
                if net.level_at(sample(f64::from(1 + i))) {
                    value |= 1 << i;
                }
            }
            let mut parity_error = false;
            if fmt.parity != Parity::None {
                let bit = net.level_at(sample(f64::from(1 + fmt.data_bits)));
                parity_error = bit != fmt.parity_of(value);
            }
            let framing_error = !net.level_at(stop_at);
            out.push(RxByte {
                at: start,
                value: value as u8,
                framing_error,
                parity_error,
            });
            self.cursor = stop_at;
        }
    }
}

/// I²C, as a master drives it at `hz`, with open-drain lines that idle
/// high. `t` advances as bits are written.
pub struct I2cWriter<'a> {
    pub scl: &'a mut EdgeLog,
    pub sda: &'a mut EdgeLog,
    pub t: Ns,
    /// A quarter of the clock period.
    q: Ns,
    /// Insert a clock glitch during the SCL-high phase of this bit (counted
    /// over every bit written, ACK slots included).
    pub glitch_at_bit: Option<usize>,
    bits_written: usize,
    /// Set once the glitch has been drawn.
    pub glitched: bool,
}

/// How long the glitch's dip on SCL lasts.
pub const SCL_GLITCH_NS: Ns = 90;

impl<'a> I2cWriter<'a> {
    pub fn new(scl: &'a mut EdgeLog, sda: &'a mut EdgeLog, t: Ns, hz: u32) -> Self {
        Self {
            scl,
            sda,
            t,
            q: 1_000_000_000 / (u64::from(hz) * 4),
            glitch_at_bit: None,
            bits_written: 0,
            glitched: false,
        }
    }

    pub fn start(&mut self) {
        // From idle (both high) or after an ACK with SCL low (a repeated
        // start): release SDA, raise SCL, then pull SDA low while SCL is high.
        self.sda.set(self.t, true);
        self.t += self.q;
        self.scl.set(self.t, true);
        self.t += self.q;
        self.sda.set(self.t, false);
        self.t += self.q;
        self.scl.set(self.t, false);
        self.t += self.q;
    }

    /// One bit: SDA changes while SCL is low, is read while it is high.
    pub fn bit(&mut self, level: bool) {
        self.sda.set(self.t, level);
        self.t += self.q;
        self.scl.set(self.t, true);
        if self.glitch_at_bit == Some(self.bits_written) {
            let g = self.t + self.q - SCL_GLITCH_NS / 2;
            self.scl.set(g, false);
            self.scl.set(g + SCL_GLITCH_NS, true);
            self.glitched = true;
        }
        self.t += 2 * self.q;
        self.scl.set(self.t, false);
        self.t += self.q;
        self.bits_written += 1;
    }

    /// Eight bits MSB first, then the ACK slot driven by `ack` (true = ACK,
    /// SDA low).
    pub fn byte(&mut self, value: u8, ack: bool) {
        for i in (0..8).rev() {
            self.bit((value >> i) & 1 == 1);
        }
        self.bit(!ack);
    }

    pub fn stop(&mut self) {
        self.sda.set(self.t, false);
        self.t += self.q;
        self.scl.set(self.t, true);
        self.t += self.q;
        self.sda.set(self.t, true);
        self.t += self.q;
    }
}

/// SPI mode 0, MSB first, chip select active low.
pub struct SpiWriter<'a> {
    pub sck: &'a mut EdgeLog,
    pub mosi: &'a mut EdgeLog,
    pub cs: &'a mut EdgeLog,
    pub t: Ns,
    half: Ns,
}

/// How long chip select bounces high in the `spi_cs_glitch` fault.
pub const CS_GLITCH_NS: Ns = 250;

impl<'a> SpiWriter<'a> {
    pub fn new(
        sck: &'a mut EdgeLog,
        mosi: &'a mut EdgeLog,
        cs: &'a mut EdgeLog,
        t: Ns,
        hz: u32,
    ) -> Self {
        Self {
            sck,
            mosi,
            cs,
            t,
            half: 1_000_000_000 / (u64::from(hz) * 2),
        }
    }

    pub fn select(&mut self) {
        self.cs.set(self.t, false);
        self.t += 2 * self.half;
    }

    pub fn byte(&mut self, value: u8) {
        for i in (0..8).rev() {
            self.mosi.set(self.t, (value >> i) & 1 == 1);
            self.t += self.half;
            self.sck.set(self.t, true);
            self.t += self.half;
            self.sck.set(self.t, false);
        }
        // A short gap between bytes, as firmware loading the next one leaves.
        self.t += self.half;
    }

    /// Chip select lets go for a moment mid-transfer.
    pub fn cs_glitch(&mut self) {
        self.cs.set(self.t, true);
        self.cs.set(self.t + CS_GLITCH_NS, false);
        self.t += CS_GLITCH_NS + self.half;
    }

    pub fn deselect(&mut self) {
        self.mosi.set(self.t, false);
        self.t += self.half;
        self.cs.set(self.t, true);
        self.t += 2 * self.half;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(net: &EdgeLog, fmt: UartFormat) -> Vec<RxByte> {
        let mut rx = UartReceiver::new(fmt, 0);
        let mut out = Vec::new();
        rx.poll(net, u64::MAX / 2, &mut out);
        out
    }

    #[test]
    fn a_written_frame_reads_back() {
        let fmt = UartFormat::new_8n1(115_200.0);
        let mut net = EdgeLog::new(true);
        let mut t = 1_000;
        for &b in b"hello" {
            t = uart_byte(&mut net, t, &fmt, b);
        }
        let got: Vec<u8> = read_all(&net, fmt).iter().map(|b| b.value).collect();
        assert_eq!(got, b"hello");
    }

    #[test]
    fn parity_round_trips_and_is_checked() {
        let fmt = UartFormat {
            parity: Parity::Even,
            ..UartFormat::new_8n1(9_600.0)
        };
        let mut net = EdgeLog::new(true);
        uart_byte(&mut net, 1_000, &fmt, 0x37);
        let got = read_all(&net, fmt);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].value, 0x37);
        assert!(!got[0].parity_error);
        let odd = UartFormat {
            parity: Parity::Odd,
            ..fmt
        };
        assert!(read_all(&net, odd)[0].parity_error);
    }

    #[test]
    fn a_fast_transmitter_garbles_back_to_back_bytes() {
        // 7 % fast: the receiver samples the stop bit inside the next start bit.
        let tx = UartFormat::new_8n1(115_200.0 * 1.07);
        let mut net = EdgeLog::new(true);
        let mut t = 1_000;
        for &b in b"UUUUUUUU" {
            t = uart_byte(&mut net, t, &tx, b);
        }
        let got = read_all(&net, UartFormat::new_8n1(115_200.0));
        assert!(got.iter().any(|b| b.framing_error || b.value != b'U'));
    }

    #[test]
    fn a_frame_is_held_back_until_it_is_complete() {
        let fmt = UartFormat::new_8n1(115_200.0);
        let mut net = EdgeLog::new(true);
        let end = uart_byte(&mut net, 1_000, &fmt, b'x');
        let mut rx = UartReceiver::new(fmt, 0);
        let mut out = Vec::new();
        rx.poll(&net, end / 2, &mut out);
        assert!(out.is_empty());
        rx.poll(&net, end, &mut out);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn i2c_writes_start_and_stop_conditions() {
        let mut scl = EdgeLog::new(true);
        let mut sda = EdgeLog::new(true);
        let mut w = I2cWriter::new(&mut scl, &mut sda, 0, 100_000);
        w.start();
        w.byte(0x90, true);
        w.stop();
        let end = w.t;
        // Start: SDA falls while SCL is high.
        let fall = sda.next_edge_to(0, false).unwrap();
        assert!(scl.level_at(fall));
        // Stop: SDA rises while SCL is high, and both idle high after.
        assert!(scl.level_at(end) && sda.level_at(end));
    }
}
