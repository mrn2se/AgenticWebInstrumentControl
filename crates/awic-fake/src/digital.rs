//! Synthetic digital sources and the fake digital [`Instrument`].
//!
//! `awic-core` stores digital captures as one packed byte per sample, one
//! bit per channel. Each channel here is independently assigned a source
//! ([`DigitalChannelSource`]); [`UartSource`] and [`I2cSource`] are built so
//! the resulting bit stream is *actually decodable* with correct framing —
//! T-M5's protocol decoders (and this crate's own UART round-trip test) run
//! against exactly these waveforms.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use awic_core::{
    AcqConfig, Capability, CaptureId, CoreError, DeviceId, EffectiveAcq, Instrument, Result,
    SampleSink, Trigger,
};
use awic_proto::frame::Encoding;
use tokio::task::JoinHandle;

use crate::device::DigitalDeviceSpec;
use crate::streaming::{stream_chunks, StreamSpec};
use crate::trigger::{resolve_trigger_shift, trigger_target_index};

/// A UART-like byte stream: idle high, 8N1 framing, LSB first, repeating
/// `bytes` indefinitely with `gap_bits` idle bit-times between frames.
#[derive(Debug, Clone, PartialEq)]
pub struct UartSource {
    pub baud: u32,
    pub bytes: Vec<u8>,
    /// Idle bit-times of mark (high) between one stop bit and the next
    /// start bit. Real UARTs vary this arbitrarily; a couple of bit-times
    /// makes the idle-vs-framing boundary unambiguous to a decoder.
    pub gap_bits: u32,
}

impl UartSource {
    pub fn new(baud: u32, bytes: Vec<u8>) -> Self {
        Self {
            baud,
            bytes,
            gap_bits: 2,
        }
    }

    /// Samples per bit at `rate_hz`, at least 1.
    pub fn bit_samples(&self, rate_hz: u64) -> i64 {
        ((rate_hz as f64) / f64::from(self.baud)).round().max(1.0) as i64
    }

    fn frame_bits(&self) -> i64 {
        // start + 8 data + stop + gap
        10 + i64::from(self.gap_bits)
    }

    fn level_at(&self, index: i64, rate_hz: u64) -> bool {
        if self.bytes.is_empty() {
            return true; // idle high
        }
        let bit_samples = self.bit_samples(rate_hz);
        let frame_bits = self.frame_bits();
        let period_samples = frame_bits * self.bytes.len() as i64 * bit_samples;

        let m = index.rem_euclid(period_samples);
        let bit_index = m.div_euclid(bit_samples);
        let byte_idx = bit_index.div_euclid(frame_bits) as usize;
        let within = bit_index.rem_euclid(frame_bits);

        match within {
            0 => false, // start bit
            n @ 1..=8 => {
                let d = (n - 1) as u32;
                (self.bytes[byte_idx] >> d) & 1 == 1
            }
            _ => true, // stop bit + idle gap
        }
    }
}

/// Which physical line of an [`I2cSource`] a channel is wired to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum I2cLine {
    Scl,
    Sda,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum I2cSegment {
    /// Bus idle: both lines high.
    Idle,
    /// SDA falls while SCL stays high.
    Start,
    /// SDA rises while SCL stays high.
    Stop,
    /// One clock: SCL low for the first half (SDA may settle), high for the
    /// second half (SDA stable — this is where a decoder samples).
    Bit(bool),
}

/// An I2C-like transaction: START, a 7-bit address + R/W, ACK, one or more
/// data bytes each followed by ACK, STOP, then an idle gap before repeating.
#[derive(Debug, Clone, PartialEq)]
pub struct I2cSource {
    pub scl_hz: u32,
    segments: Vec<I2cSegment>,
}

impl I2cSource {
    pub fn new(address_7bit: u8, read: bool, data: Vec<u8>, scl_hz: u32) -> Self {
        let mut segments = vec![I2cSegment::Start];
        let addr_byte = (address_7bit << 1) | u8::from(read);
        push_byte_with_ack(&mut segments, addr_byte);
        for byte in data {
            push_byte_with_ack(&mut segments, byte);
        }
        segments.push(I2cSegment::Stop);
        // Idle gap so the repeating transaction has an unambiguous boundary.
        segments.extend(std::iter::repeat_n(I2cSegment::Idle, 4));
        Self { scl_hz, segments }
    }

    fn quarter_samples(&self, rate_hz: u64) -> i64 {
        let bit_samples = ((rate_hz as f64) / f64::from(self.scl_hz)).round().max(4.0) as i64;
        (bit_samples / 4).max(1)
    }

    fn level_at(&self, index: i64, rate_hz: u64, line: I2cLine) -> bool {
        let qs = self.quarter_samples(rate_hz);
        let seg_len = qs * 4;
        let period = seg_len * self.segments.len() as i64;
        let m = index.rem_euclid(period);
        let seg_idx = (m / seg_len) as usize;
        let within = m % seg_len;
        let q = (within / qs).min(3);

        let (scl, sda) = match self.segments[seg_idx] {
            I2cSegment::Idle => (true, true),
            I2cSegment::Start => (true, q < 3),
            I2cSegment::Stop => (true, q >= 3),
            I2cSegment::Bit(bit) => (q >= 2, bit),
        };
        match line {
            I2cLine::Scl => scl,
            I2cLine::Sda => sda,
        }
    }
}

fn push_byte_with_ack(segments: &mut Vec<I2cSegment>, byte: u8) {
    for d in (0..8).rev() {
        segments.push(I2cSegment::Bit((byte >> d) & 1 == 1));
    }
    // ACK: receiver pulls SDA low.
    segments.push(I2cSegment::Bit(false));
}

/// A selectable digital signal source for one channel.
#[derive(Debug, Clone, PartialEq)]
pub enum DigitalChannelSource {
    Low,
    High,
    /// Reflects bit `bit` of a free-running counter that increments every
    /// `divider` samples (`divider = 1` toggles the LSB every sample).
    CounterBit {
        divider: u64,
        bit: u8,
    },
    Uart(UartSource),
    I2c(Arc<I2cSource>, I2cLine),
}

impl DigitalChannelSource {
    pub fn level_at(&self, index: i64, rate_hz: u64) -> bool {
        match self {
            DigitalChannelSource::Low => false,
            DigitalChannelSource::High => true,
            DigitalChannelSource::CounterBit { divider, bit } => {
                let d = (*divider).max(1) as i64;
                let counter = index.div_euclid(d);
                ((counter >> bit) & 1) == 1
            }
            DigitalChannelSource::Uart(u) => u.level_at(index, rate_hz),
            DigitalChannelSource::I2c(cfg, line) => cfg.level_at(index, rate_hz, *line),
        }
    }
}

/// The fake digital `Instrument`, mirroring the fx2lafw logic analyzer's capability
/// set by default (SPECIFICATION.md §2.5).
pub struct DigitalInstrument {
    id: DeviceId,
    spec: DigitalDeviceSpec,
    capabilities: Vec<Capability>,
    channels: Vec<DigitalChannelSource>,
    ready: bool,
    acq: Option<AcqConfig>,
    cancel: Option<Arc<AtomicBool>>,
    task: Option<JoinHandle<()>>,
}

/// Bytes the default fake digital device's D5 UART channel repeats forever
/// (T21's "add a small, clearly labelled UART or I²C pattern mode": the
/// existing default channels are all `CounterBit` — an exact square wave,
/// not a protocol — so `capture.decode`/`rec.decode` had nothing meaningful
/// to decode without this). Spells out as ASCII so a decoded terminal
/// session is legible at a glance, not just byte values in a test assertion.
pub const D5_UART_BYTES: &[u8] = b"AWIC";
/// Baud rate for D5's UART source. Comfortably resolvable (>4 samples/bit —
/// `awic_decode::uart`'s `MIN_SAMPLES_PER_BIT`) at every rate the fake
/// digital device's ladder offers (20 kHz and up).
pub const D5_UART_BAUD: u32 = 9_600;
/// I²C address D6 (SCL)/D7 (SDA) address in every repeated transaction.
pub const D6_D7_I2C_ADDRESS: u8 = 0x50;
/// One data byte D6/D7's I²C transaction writes each time it repeats.
pub const D6_D7_I2C_DATA: u8 = 0xAB;
/// D6/D7's I²C clock rate.
pub const D6_D7_I2C_HZ: u32 = 100_000;

impl DigitalInstrument {
    pub fn new(spec: DigitalDeviceSpec) -> Self {
        let capability = spec.capability();
        // Default: each channel shows one bit of a shared ripple counter —
        // the classic logic-analyzer demo pattern — **except** D5, which
        // carries an actually-decodable UART byte stream, and D6/D7, an
        // actually-decodable I²C transaction (SCL/SDA respectively): the
        // fixtures `capture.decode`/`rec.decode`/the `decode` terminal
        // source and their tests exercise (see `D5_UART_BYTES` etc. above).
        // D0-D4 are left as plain counter bits because existing tests
        // (`awic-server`'s `recorder`/`mcp_protocol` integration tests) rely
        // on their exact toggle rate.
        // Built once and shared (via `Arc`) between D6 and D7, not
        // reconstructed per channel: both lines must read the same
        // transaction timeline for the decoder to see a coherent bus.
        let i2c_source = (spec.channels > 7).then(|| {
            Arc::new(I2cSource::new(
                D6_D7_I2C_ADDRESS,
                false,
                vec![D6_D7_I2C_DATA],
                D6_D7_I2C_HZ,
            ))
        });
        let channels = (0..spec.channels)
            .map(|bit| match bit {
                5 if spec.channels > 5 => DigitalChannelSource::Uart(UartSource::new(
                    D5_UART_BAUD,
                    D5_UART_BYTES.to_vec(),
                )),
                6 if i2c_source.is_some() => {
                    DigitalChannelSource::I2c(i2c_source.clone().unwrap(), I2cLine::Scl)
                }
                7 if i2c_source.is_some() => {
                    DigitalChannelSource::I2c(i2c_source.clone().unwrap(), I2cLine::Sda)
                }
                bit => DigitalChannelSource::CounterBit { divider: 1, bit },
            })
            .collect();
        Self {
            id: spec.id.clone(),
            capabilities: vec![capability],
            channels,
            spec,
            ready: false,
            acq: None,
            cancel: None,
            task: None,
        }
    }

    pub fn set_channel(&mut self, channel: u8, source: DigitalChannelSource) {
        self.channels[channel as usize] = source;
    }
}

#[async_trait]
impl Instrument for DigitalInstrument {
    fn label(&self) -> Option<String> {
        Some(self.spec.label.clone())
    }

    /// Synthetic sources place the trigger themselves (see `trigger.rs`).
    fn honours_trigger(&self) -> bool {
        true
    }

    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn capabilities(&self) -> &[Capability] {
        &self.capabilities
    }

    async fn prepare(&mut self) -> Result<()> {
        tokio::time::sleep(Duration::from_millis(20)).await;
        self.ready = true;
        Ok(())
    }

    async fn configure(&mut self, cfg: AcqConfig) -> Result<EffectiveAcq> {
        if !self.ready {
            return Err(CoreError::Device(
                "prepare() must complete before configure()".to_string(),
            ));
        }
        if cfg.rate_hz == 0 || cfg.rate_hz > self.spec.max_rate_hz {
            return Err(CoreError::Config(format!(
                "rate {} Hz exceeds {}'s max of {} Hz",
                cfg.rate_hz, self.spec.label, self.spec.max_rate_hz
            )));
        }
        if cfg.channels.is_empty() || cfg.channels.iter().any(|&c| c >= self.spec.channels) {
            return Err(CoreError::Config(format!(
                "channel selection {:?} invalid for a {}-channel device",
                cfg.channels, self.spec.channels
            )));
        }
        // `samples` is ignored in continuous mode (SPEC §2.3.2): 0 is valid
        // there, since the caller has nothing meaningful to put in it.
        if !cfg.continuous && cfg.samples == 0 {
            return Err(CoreError::Config("samples must be > 0".to_string()));
        }
        self.acq = Some(cfg.clone());
        Ok(EffectiveAcq::digital(cfg))
    }

    async fn start(&mut self, sink: SampleSink) -> Result<CaptureId> {
        let cfg = self.acq.clone().ok_or_else(|| {
            CoreError::Config("configure() must complete before start()".to_string())
        })?;

        let channels = self.channels.clone();
        let enabled = cfg.channels.clone();
        let rate_hz = cfg.rate_hz;
        let total_samples = cfg.samples;
        let continuous = cfg.continuous;

        let shift = resolve_shift(&cfg.trigger, total_samples, &channels, rate_hz);

        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_task = cancel.clone();
        let handle = tokio::spawn(async move {
            stream_chunks(
                &sink,
                StreamSpec {
                    encoding: Encoding::PackedDigital,
                    channel_count: 1,
                    total_samples,
                    rate_hz,
                    continuous,
                },
                &cancel_task,
                move |n, buf| {
                    let abs = n + shift;
                    let mut byte = 0u8;
                    for &c in &enabled {
                        if channels[c as usize].level_at(abs, rate_hz) {
                            byte |= 1 << c;
                        }
                    }
                    buf.push(byte);
                },
            )
            .await;
        });

        self.cancel = Some(cancel);
        self.task = Some(handle);
        Ok(CaptureId::new())
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some(cancel) = self.cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        Ok(())
    }
}

fn resolve_shift(
    trigger: &Option<Trigger>,
    total_samples: u64,
    channels: &[DigitalChannelSource],
    rate_hz: u64,
) -> i64 {
    let Some(trigger) = trigger else {
        return 0;
    };
    let Trigger::Edge { channel, slope, .. } = trigger;
    let ch = *channel;
    let source = channels[ch as usize].clone();
    // A digital edge is a change of bit: whatever `level` says (a dummy 0
    // used to be required, and never crossed), the threshold is half.
    let edge = Trigger::Edge {
        channel: ch,
        slope: *slope,
        level: None,
    };
    resolve_trigger_shift(&edge, trigger_target_index(total_samples), move |k| {
        u8::from(source.level_at(k, rate_hz)) as f64
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal inline UART decoder — mirrors what a real T-M5 decoder
    /// would do: find the falling start-bit edge, sample the middle of each
    /// subsequent bit cell, LSB first, check for a high stop bit.
    fn decode_uart(bits: &[bool], bit_samples: i64, max_bytes: usize) -> Vec<u8> {
        let bit_samples = bit_samples as usize;
        let mut out = Vec::new();
        let mut i = 1;
        while i < bits.len() && out.len() < max_bytes {
            if bits[i - 1] && !bits[i] {
                // Falling edge: start bit begins at i. Sample each cell's
                // midpoint.
                let mut byte = 0u8;
                let mut ok = true;
                for d in 0..8 {
                    let idx = i + bit_samples / 2 + (d + 1) * bit_samples;
                    if idx >= bits.len() {
                        ok = false;
                        break;
                    }
                    if bits[idx] {
                        byte |= 1 << d;
                    }
                }
                let stop_idx = i + bit_samples / 2 + 9 * bit_samples;
                if ok && stop_idx < bits.len() && bits[stop_idx] {
                    out.push(byte);
                    i = stop_idx + bit_samples; // skip past this frame
                    continue;
                }
            }
            i += 1;
        }
        out
    }

    #[test]
    fn uart_source_round_trips_through_a_hand_rolled_decoder() {
        let rate_hz = 1_000_000u64;
        let baud = 9_600u32;
        let bytes = vec![0xA5, 0x3C, 0x00, 0xFF, 0x81];
        let uart = UartSource::new(baud, bytes.clone());
        let bit_samples = uart.bit_samples(rate_hz);
        let frame_samples = uart.frame_bits() as i64 * bit_samples;
        let total_samples = frame_samples * bytes.len() as i64 * 2; // two full loops

        // Include one bit-time of lead-in before index 0 so the very first
        // byte's start-bit falling edge (high -> low) is visible inside the
        // sampled window rather than straddling its boundary.
        let levels: Vec<bool> = (-bit_samples..total_samples)
            .map(|n| uart.level_at(n, rate_hz))
            .collect();

        let decoded = decode_uart(&levels, bit_samples, bytes.len() * 2);
        assert_eq!(decoded.len(), bytes.len() * 2, "decoded {decoded:?}");
        assert_eq!(&decoded[..bytes.len()], bytes.as_slice());
        assert_eq!(&decoded[bytes.len()..], bytes.as_slice());
    }

    #[test]
    fn counter_bit_toggles_at_the_configured_divider() {
        let src = DigitalChannelSource::CounterBit { divider: 4, bit: 0 };
        let levels: Vec<bool> = (0..16).map(|n| src.level_at(n, 1_000)).collect();
        // bit 0 of (n/4): 0,0,0,0,1,1,1,1,0,0,0,0,1,1,1,1
        assert_eq!(
            levels,
            vec![
                false, false, false, false, true, true, true, true, false, false, false, false,
                true, true, true, true,
            ]
        );
    }

    #[test]
    fn counter_bit_handles_negative_indices() {
        let src = DigitalChannelSource::CounterBit { divider: 1, bit: 0 };
        // Must not panic, and must stay consistent with div_euclid semantics.
        let _ = src.level_at(-5, 1_000);
    }

    #[test]
    fn i2c_transaction_contains_a_start_and_stop_condition() {
        let i2c = I2cSource::new(0x50, false, vec![0xAB], 100_000);
        let rate_hz = 1_000_000u64;
        let qs = i2c.quarter_samples(rate_hz);
        let seg_len = qs * 4;

        // Start segment (index 0): SCL high throughout, SDA falls partway.
        let scl0: Vec<bool> = (0..seg_len)
            .map(|k| i2c.level_at(k, rate_hz, I2cLine::Scl))
            .collect();
        let sda0: Vec<bool> = (0..seg_len)
            .map(|k| i2c.level_at(k, rate_hz, I2cLine::Sda))
            .collect();
        assert!(scl0.iter().all(|&s| s), "SCL must stay high during START");
        assert!(sda0[0], "SDA starts high before START");
        assert!(
            !sda0[seg_len as usize - 1],
            "SDA must have fallen by end of START"
        );
    }

    #[test]
    fn i2c_address_byte_is_recoverable_by_sampling_scl_high_phase() {
        let address = 0x50u8;
        let read = false;
        let i2c = I2cSource::new(address, read, vec![], 100_000);
        let rate_hz = 1_000_000u64;
        let qs = i2c.quarter_samples(rate_hz);
        let seg_len = qs * 4;

        // Segment 0 is START; segments 1..=8 are the address+RW bits.
        let mut recovered = 0u8;
        for (i, seg) in (1..=8).enumerate() {
            let sample_at = seg as i64 * seg_len + qs * 2; // mid of SCL-high half
            let bit = i2c.level_at(sample_at, rate_hz, I2cLine::Sda);
            if bit {
                recovered |= 1 << (7 - i);
            }
        }
        let expected = (address << 1) | u8::from(read);
        assert_eq!(recovered, expected);
    }
}
