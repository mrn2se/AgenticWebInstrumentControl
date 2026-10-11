//! Synthetic analog waveforms and the fake analog [`Instrument`].
//!
//! Samples are produced as **raw 8-bit device values plus scale/offset**
//! (SPECIFICATION.md §2.7), never pre-converted floats: [`quantize`] maps a
//! desired voltage onto the same `raw * scale + offset` representation
//! [`awic_core::CaptureData::Analog`] stores, using the channel's selected
//! full-scale range.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use awic_core::{
    AcqConfig, Capability, CaptureId, ChannelCalibration, CoreError, DeviceId, EffectiveAcq,
    Instrument, Result, SampleSink, Trigger,
};
use awic_proto::frame::Encoding;
use tokio::task::JoinHandle;

use crate::device::AnalogDeviceSpec;
use crate::noise::gaussian;
use crate::streaming::{stream_chunks, StreamSpec};
use crate::trigger::{resolve_trigger_shift, trigger_target_index};

/// A selectable analog signal source.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AnalogWaveform {
    Sine {
        amplitude_v: f32,
        freq_hz: f64,
        offset_v: f32,
    },
    Square {
        amplitude_v: f32,
        freq_hz: f64,
        offset_v: f32,
        /// Fraction of the period spent at the high level, in `(0.0, 1.0)`.
        duty: f32,
    },
    Triangle {
        amplitude_v: f32,
        freq_hz: f64,
        offset_v: f32,
    },
    Dc {
        level_v: f32,
    },
    /// Gaussian noise with the given standard deviation, centered on
    /// `offset_v`. Distinct from `noise_floor_v` on [`AnalogChannelConfig`],
    /// which *adds* noise on top of any waveform rather than replacing it.
    Noise {
        amplitude_v: f32,
        offset_v: f32,
    },
}

impl AnalogWaveform {
    fn periodic_value(&self, t_seconds: f64) -> f64 {
        match *self {
            AnalogWaveform::Sine {
                amplitude_v,
                freq_hz,
                offset_v,
            } => {
                offset_v as f64
                    + amplitude_v as f64 * (2.0 * std::f64::consts::PI * freq_hz * t_seconds).sin()
            }
            AnalogWaveform::Square {
                amplitude_v,
                freq_hz,
                offset_v,
                duty,
            } => {
                let phase = (t_seconds * freq_hz).rem_euclid(1.0);
                let level = if phase < duty as f64 { 1.0 } else { -1.0 };
                offset_v as f64 + amplitude_v as f64 * level
            }
            AnalogWaveform::Triangle {
                amplitude_v,
                freq_hz,
                offset_v,
            } => {
                let phase = (t_seconds * freq_hz).rem_euclid(1.0);
                let tri = 4.0 * (phase - 0.5).abs() - 1.0;
                offset_v as f64 + amplitude_v as f64 * tri
            }
            AnalogWaveform::Dc { level_v } => level_v as f64,
            AnalogWaveform::Noise { offset_v, .. } => offset_v as f64,
        }
    }

    /// Voltage at absolute sample `index` (`t_seconds` = `index / rate_hz`,
    /// passed in rather than recomputed since callers already have it).
    fn value_at(&self, seed: u64, channel: u8, index: i64, t_seconds: f64) -> f64 {
        match self {
            AnalogWaveform::Noise { amplitude_v, .. } => {
                self.periodic_value(t_seconds)
                    + gaussian(seed, channel, index) * *amplitude_v as f64
            }
            _ => self.periodic_value(t_seconds),
        }
    }
}

/// Per-channel analog configuration: waveform plus an optional additive
/// noise floor and the full-scale range used to quantize to 8 bits.
#[derive(Debug, Clone, PartialEq)]
pub struct AnalogChannelConfig {
    pub waveform: AnalogWaveform,
    /// Standard deviation, in volts, of Gaussian noise added on top of
    /// `waveform`. `0.0` disables it.
    pub noise_floor_v: f32,
    /// Full-scale range (must be one of the device's `ranges_v`); samples
    /// outside `[-range_v, range_v]` clip, same as real hardware.
    pub range_v: f32,
}

impl AnalogChannelConfig {
    pub fn sine(amplitude_v: f32, freq_hz: f64, range_v: f32) -> Self {
        Self {
            waveform: AnalogWaveform::Sine {
                amplitude_v,
                freq_hz,
                offset_v: 0.0,
            },
            noise_floor_v: 0.0,
            range_v,
        }
    }

    fn value_at(&self, seed: u64, channel: u8, index: i64, t_seconds: f64) -> f64 {
        let mut v = self.waveform.value_at(seed, channel, index, t_seconds);
        if self.noise_floor_v > 0.0 {
            // A distinct hash offset so the additive floor is independent of
            // any noise the waveform itself already is.
            v += gaussian(seed ^ 0xA5A5_A5A5_A5A5_A5A5, channel, index) * self.noise_floor_v as f64;
        }
        v
    }
}

/// Maps a full-scale range onto the `(scale, offset)` pair
/// [`awic_core::CaptureData::Analog`] expects: `raw * scale + offset` recovers
/// volts, with `raw` an 8-bit unsigned device sample.
pub fn analog_scale_offset(range_v: f32) -> (f32, f32) {
    let scale = (2.0 * range_v) / 255.0;
    (scale, -range_v)
}

/// Quantizes a voltage to the raw 8-bit device value a real ADC would
/// produce for `range_v` full scale, clamping (not wrapping) out-of-range
/// values exactly as real hardware clips.
pub fn quantize(v: f64, range_v: f32) -> u8 {
    let (scale, offset) = analog_scale_offset(range_v);
    let raw = (v - offset as f64) / scale as f64;
    raw.round().clamp(0.0, 255.0) as u8
}

/// The fake analog `Instrument`, mirroring the FX2 oscilloscope's capability
/// set by default (SPECIFICATION.md §2.5).
pub struct AnalogInstrument {
    id: DeviceId,
    spec: AnalogDeviceSpec,
    capabilities: Vec<Capability>,
    channels: Vec<AnalogChannelConfig>,
    seed: u64,
    ready: bool,
    acq: Option<AcqConfig>,
    cancel: Option<Arc<AtomicBool>>,
    task: Option<JoinHandle<()>>,
}

impl AnalogInstrument {
    pub fn new(spec: AnalogDeviceSpec) -> Self {
        let capability = spec.capability();
        let default_channel = AnalogChannelConfig::sine(1.0, 1_000.0, spec.ranges_v[0]);
        let channels = vec![default_channel; spec.channels as usize];
        Self {
            id: spec.id.clone(),
            capabilities: vec![capability],
            channels,
            spec,
            seed: 0xA5A5_ADC0_0000_0001,
            ready: false,
            acq: None,
            cancel: None,
            task: None,
        }
    }

    /// Overrides a channel's signal source. Panics if `channel` is out of
    /// range for this device — a programmer error in test/setup code, not a
    /// runtime condition.
    pub fn set_channel(&mut self, channel: u8, cfg: AnalogChannelConfig) {
        self.channels[channel as usize] = cfg;
    }

    /// Fixes the noise seed, for deterministic tests that need identical
    /// output across two instruments.
    pub fn set_seed(&mut self, seed: u64) {
        self.seed = seed;
    }
}

#[async_trait]
impl Instrument for AnalogInstrument {
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
        // Simulates the FX2 flow (SPECIFICATION.md §2.4): firmware upload
        // takes real wall-clock time, and the UI's `flashing` -> `ready`
        // transition needs something to actually wait on.
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
        if !self.spec.rates_hz.contains(&cfg.rate_hz) {
            return Err(CoreError::Config(format!(
                "rate {} Hz is not supported by {} (supported: {:?})",
                cfg.rate_hz, self.spec.label, self.spec.rates_hz
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
        if !cfg.ranges_v.is_empty() {
            if cfg.ranges_v.len() != cfg.channels.len() {
                return Err(CoreError::Config(format!(
                    "{} ranges given for {} channels",
                    cfg.ranges_v.len(),
                    cfg.channels.len()
                )));
            }
            if let Some(bad) = cfg
                .ranges_v
                .iter()
                .find(|r| !self.spec.ranges_v.contains(r))
            {
                return Err(CoreError::Config(format!(
                    "range ±{bad} V is not supported by {} (supported: {:?})",
                    self.spec.label, self.spec.ranges_v
                )));
            }
            for (&ch, &range_v) in cfg.channels.iter().zip(&cfg.ranges_v) {
                self.channels[ch as usize].range_v = range_v;
            }
        }

        // Report the ranges as applied — including channels the request left
        // at their default — and the matching raw-to-volts conversion.
        let mut effective = cfg;
        effective.ranges_v = effective
            .channels
            .iter()
            .map(|&ch| self.channels[ch as usize].range_v)
            .collect();
        let calibration = effective
            .channels
            .iter()
            .zip(&effective.ranges_v)
            .map(|(&channel, &range_v)| {
                let (scale, offset) = analog_scale_offset(range_v);
                ChannelCalibration {
                    channel,
                    scale,
                    offset,
                    unit: "V".to_string(),
                    range_v: Some(range_v),
                }
            })
            .collect();
        self.acq = Some(effective.clone());
        Ok(EffectiveAcq {
            cfg: effective,
            calibration,
        })
    }

    async fn start(&mut self, sink: SampleSink) -> Result<CaptureId> {
        let cfg = self.acq.clone().ok_or_else(|| {
            CoreError::Config("configure() must complete before start()".to_string())
        })?;

        let channels = self.channels.clone();
        let active = cfg.channels.clone();
        let rate_hz = cfg.rate_hz;
        let total_samples = cfg.samples;
        let continuous = cfg.continuous;
        let seed = self.seed;

        let shift = resolve_shift(&cfg.trigger, total_samples, &channels, rate_hz, seed);

        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_task = cancel.clone();
        let handle = tokio::spawn(async move {
            let channel_count = active.len() as u8;
            stream_chunks(
                &sink,
                StreamSpec {
                    encoding: Encoding::U8Interleaved,
                    channel_count,
                    total_samples,
                    rate_hz,
                    continuous,
                },
                &cancel_task,
                move |n, buf| {
                    let abs = n + shift;
                    let t = abs as f64 / rate_hz as f64;
                    for &c in &active {
                        let cfgc = &channels[c as usize];
                        let v = cfgc.value_at(seed, c, abs, t);
                        buf.push(quantize(v, cfgc.range_v));
                    }
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

/// Resolves the trigger shift (or `0` if untriggered) for a start() call,
/// pulled out of the method body so it reads as one step rather than a
/// nested closure-inside-closure.
fn resolve_shift(
    trigger: &Option<Trigger>,
    total_samples: u64,
    channels: &[AnalogChannelConfig],
    rate_hz: u64,
    seed: u64,
) -> i64 {
    let Some(trigger) = trigger else {
        return 0;
    };
    let Trigger::Edge { channel, .. } = trigger;
    let ch = *channel;
    let cfgc = channels[ch as usize].clone();
    resolve_trigger_shift(trigger, trigger_target_index(total_samples), move |k| {
        cfgc.value_at(seed, ch, k, k as f64 / rate_hz as f64)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantize_maps_full_scale_endpoints() {
        assert_eq!(quantize(-5.0, 5.0), 0);
        assert_eq!(quantize(5.0, 5.0), 255);
        // Midpoint rounds to the nearest raw value near 127/128.
        let mid = quantize(0.0, 5.0);
        assert!((127..=128).contains(&mid));
    }

    #[test]
    fn quantize_clips_out_of_range_like_real_hardware() {
        assert_eq!(quantize(-100.0, 5.0), 0);
        assert_eq!(quantize(100.0, 5.0), 255);
    }

    #[test]
    fn scale_offset_round_trips_quantize() {
        let (scale, offset) = analog_scale_offset(5.0);
        let raw = quantize(1.234, 5.0);
        let recovered = raw as f32 * scale + offset;
        assert!((recovered - 1.234).abs() < scale, "recovered {recovered}");
    }
}
