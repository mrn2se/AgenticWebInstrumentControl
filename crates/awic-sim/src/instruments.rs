//! The virtual instruments: two logic analyzers and an oscilloscope, each
//! an [`Instrument`] that samples the board's nets in real time.
//!
//! They behave like the real FX2 drivers rather than like `awic-fake`: they
//! stream continuously from "now", cannot trigger themselves (the server's
//! software trigger finds the edge), and stop when their hub port loses
//! power. Samples are computed only once the wall clock has passed them, so
//! a key typed into the console shows up on the analyzer's TX channel.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use awic_core::{
    AcqConfig, Backend, Capability, CaptureId, ChannelCalibration, CoreError, DeviceId,
    EffectiveAcq, Instrument, Result, SampleChunk, SampleSink,
};
use awic_proto::frame::Encoding;
use bytes::Bytes;
use tokio::task::JoinHandle;

use crate::bench::{ScopeProbe, SimBench, LOGIC2_LABEL, LOGIC_LABEL, SCOPE_LABEL};
use crate::net::{Ns, Window};
use crate::rng::noise;
use crate::world::{CapSegment, Net, OSC_OUT_TAU_NS, PORT_LOGIC, PORT_LOGIC2, PORT_SCOPE};

const LOGIC_RATES: [u64; 16] = [
    20_000, 25_000, 50_000, 100_000, 200_000, 250_000, 500_000, 1_000_000, 2_000_000, 3_000_000,
    4_000_000, 6_000_000, 8_000_000, 12_000_000, 16_000_000, 24_000_000,
];
const SCOPE_RATES: [u64; 10] = [
    48_000_000, 30_000_000, 24_000_000, 16_000_000, 8_000_000, 4_000_000, 1_000_000, 500_000,
    200_000, 100_000,
];
const SCOPE_RANGES: [f32; 4] = [5.0, 2.5, 1.25, 0.5];
/// The scope's front-end noise, volts RMS.
const SCOPE_NOISE_V: f64 = 0.006;

/// About this many chunks a second, like the native drivers' transfers.
const CHUNKS_PER_SECOND: u64 = 50;

/// The backend presenting the virtual instruments that are powered.
pub struct SimBackend {
    bench: SimBench,
}

impl SimBackend {
    pub fn new(bench: SimBench) -> Self {
        Self { bench }
    }
}

#[async_trait]
impl Backend for SimBackend {
    fn name(&self) -> &str {
        "sim"
    }

    async fn discover(&self) -> Result<Vec<DeviceId>> {
        Ok(self.bench.powered_instruments())
    }

    async fn open(&self, id: &DeviceId) -> Result<Box<dyn Instrument>> {
        let bench = self.bench.clone();
        if *id == SimBench::logic_id() {
            let nets = bench.wiring().logic;
            Ok(Box::new(SimLogic::new(
                bench,
                id.clone(),
                PORT_LOGIC,
                LOGIC_LABEL,
                nets,
            )))
        } else if *id == SimBench::logic2_id() {
            let nets = bench.wiring().logic2;
            Ok(Box::new(SimLogic::new(
                bench,
                id.clone(),
                PORT_LOGIC2,
                LOGIC2_LABEL,
                nets,
            )))
        } else if *id == SimBench::scope_id() {
            Ok(Box::new(SimScope::new(bench, id.clone())))
        } else {
            Err(CoreError::DeviceNotFound(id.to_string()))
        }
    }
}

/// The time of sample `n` of a capture that started at `start`.
fn sample_time(start: Ns, n: u64, rate_hz: u64) -> Ns {
    start + (u128::from(n) * 1_000_000_000 / u128::from(rate_hz.max(1))) as Ns
}

fn chunk_len(rate_hz: u64) -> u64 {
    (rate_hz / CHUNKS_PER_SECOND).clamp(256, 1 << 19)
}

/// Waits until the bench clock has passed `t`.
async fn wait_for(bench: &SimBench, t: Ns) {
    let now = bench.now_ns();
    if t > now {
        tokio::time::sleep(Duration::from_nanos(t - now)).await;
    }
}

/// Streams chunks until stopped, the capture is complete, the port loses
/// power or nobody is listening. `fill(first_sample, count, buf)` writes the
/// samples' bytes.
async fn stream(
    bench: SimBench,
    port: u8,
    sink: SampleSink,
    cancel: Arc<AtomicBool>,
    spec: (Encoding, u8, u64, u64, bool),
    mut fill: impl FnMut(&SimBench, Ns, u64, u64, &mut Vec<u8>) -> bool + Send,
) {
    let (encoding, channel_count, total, rate_hz, continuous) = spec;
    let start = bench.now_ns();
    let len = chunk_len(rate_hz);
    let mut sent = 0u64;
    while continuous || sent < total {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let this = if continuous {
            len
        } else {
            len.min(total - sent)
        };
        let last = sample_time(start, sent + this - 1, rate_hz);
        wait_for(&bench, last).await;
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let mut buf = Vec::with_capacity((this * u64::from(channel_count.max(1))) as usize);
        if !fill(&bench, start, sent, this, &mut buf) {
            tracing::info!(port, "virtual instrument lost power; acquisition ends");
            break;
        }
        let chunk = SampleChunk {
            encoding,
            channel_count,
            sample_count: this as u32,
            t0_ns: sample_time(0, sent, rate_hz),
            data: Bytes::from(buf),
        };
        if sink.send(chunk).await.is_err() {
            break;
        }
        sent += this;
    }
}

// ---- logic analyzer ---------------------------------------------------------

pub struct SimLogic {
    bench: SimBench,
    id: DeviceId,
    port: u8,
    label: &'static str,
    nets: [Net; 8],
    capabilities: Vec<Capability>,
    ready: bool,
    acq: Option<AcqConfig>,
    cancel: Option<Arc<AtomicBool>>,
    task: Option<JoinHandle<()>>,
}

impl SimLogic {
    fn new(bench: SimBench, id: DeviceId, port: u8, label: &'static str, nets: [Net; 8]) -> Self {
        Self {
            bench,
            id,
            port,
            label,
            nets,
            capabilities: vec![Capability::Digital {
                channels: 8,
                max_rate_hz: 24_000_000,
                rates_hz: LOGIC_RATES.to_vec(),
            }],
            ready: false,
            acq: None,
            cancel: None,
            task: None,
        }
    }
}

/// Packs the logic levels of `windows` (one per enabled channel bit) at each
/// sample time into bytes.
fn pack_digital(
    windows: &[(u8, Window)],
    start: Ns,
    first: u64,
    count: u64,
    rate_hz: u64,
    buf: &mut Vec<u8>,
) {
    let mut cursors: Vec<(u8, bool, usize, &[Ns])> = windows
        .iter()
        .map(|(bit, w)| (*bit, w.level, 0usize, w.edges.as_slice()))
        .collect();
    for n in first..first + count {
        let t = sample_time(start, n, rate_hz);
        let mut byte = 0u8;
        for (bit, level, idx, edges) in cursors.iter_mut() {
            while *idx < edges.len() && edges[*idx] <= t {
                *level = !*level;
                *idx += 1;
            }
            if *level {
                byte |= 1 << *bit;
            }
        }
        buf.push(byte);
    }
}

#[async_trait]
impl Instrument for SimLogic {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn capabilities(&self) -> &[Capability] {
        &self.capabilities
    }

    fn label(&self) -> Option<String> {
        Some(self.label.to_string())
    }

    async fn prepare(&mut self) -> Result<()> {
        tokio::time::sleep(Duration::from_millis(30)).await;
        self.ready = true;
        Ok(())
    }

    async fn configure(&mut self, cfg: AcqConfig) -> Result<EffectiveAcq> {
        if !self.ready {
            return Err(CoreError::Device("prepare() must complete first".into()));
        }
        if !self.bench.port_power(self.port) {
            return Err(CoreError::Device(format!("{} has no power", self.label)));
        }
        if !LOGIC_RATES.contains(&cfg.rate_hz) {
            return Err(CoreError::Config(format!(
                "rate {} Hz is not supported by {} (supported: {:?})",
                cfg.rate_hz, self.label, LOGIC_RATES
            )));
        }
        if cfg.channels.is_empty() || cfg.channels.iter().any(|&c| c >= 8) {
            return Err(CoreError::Config(format!(
                "channel selection {:?} invalid for an 8-channel device",
                cfg.channels
            )));
        }
        if !cfg.continuous && cfg.samples == 0 {
            return Err(CoreError::Config("samples must be > 0".into()));
        }
        self.acq = Some(cfg.clone());
        Ok(EffectiveAcq::digital(cfg))
    }

    async fn start(&mut self, sink: SampleSink) -> Result<CaptureId> {
        let cfg = self
            .acq
            .clone()
            .ok_or_else(|| CoreError::Config("configure() must complete first".into()))?;
        let rate = cfg.rate_hz;
        let port = self.port;
        let channels: Vec<(u8, Net)> = cfg
            .channels
            .iter()
            .map(|&c| (c, self.nets[usize::from(c)]))
            .collect();
        let cancel = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(stream(
            self.bench.clone(),
            port,
            sink,
            cancel.clone(),
            (
                Encoding::PackedDigital,
                1,
                cfg.samples,
                rate,
                cfg.continuous,
            ),
            move |bench, start, first, count, buf| {
                let t0 = sample_time(start, first, rate);
                let t1 = sample_time(start, first + count - 1, rate);
                let windows = bench.read_at(t1, |w| {
                    w.port_power(port).then(|| {
                        channels
                            .iter()
                            .map(|&(bit, net)| {
                                (bit, w.digital_window(net, t0.saturating_sub(1), t1))
                            })
                            .collect::<Vec<_>>()
                    })
                });
                let Some(windows) = windows else {
                    return false;
                };
                pack_digital(&windows, start, first, count, rate, buf);
                true
            },
        ));
        self.cancel = Some(cancel);
        self.task = Some(task);
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

// ---- oscilloscope -----------------------------------------------------------

pub struct SimScope {
    bench: SimBench,
    id: DeviceId,
    capabilities: Vec<Capability>,
    ranges: [f32; 2],
    ready: bool,
    acq: Option<AcqConfig>,
    cancel: Option<Arc<AtomicBool>>,
    task: Option<JoinHandle<()>>,
}

impl SimScope {
    fn new(bench: SimBench, id: DeviceId) -> Self {
        Self {
            bench,
            id,
            capabilities: vec![Capability::Analog {
                channels: 2,
                bits: 8,
                rates_hz: SCOPE_RATES.to_vec(),
                ranges_v: SCOPE_RANGES.to_vec(),
            }],
            ranges: [5.0, 5.0],
            ready: false,
            acq: None,
            cancel: None,
            task: None,
        }
    }
}

/// `raw * scale + offset` recovers volts from an 8-bit code at `range_v`.
fn scale_offset(range_v: f32) -> (f32, f32) {
    ((2.0 * range_v) / 255.0, -range_v)
}

fn quantize(v: f64, range_v: f32) -> u8 {
    let (scale, offset) = scale_offset(range_v);
    ((v - f64::from(offset)) / f64::from(scale))
        .round()
        .clamp(0.0, 255.0) as u8
}

/// What one scope channel needs to compute its samples for a chunk.
enum ProbeData {
    Cap(Vec<CapSegment>),
    /// The output's edges from well before the chunk (so the slew in
    /// progress at its start is exact), and the level they start from.
    Out {
        from: Ns,
        window: Window,
        high_v: f64,
    },
}

/// Voltages of one channel at the chunk's sample times.
fn probe_voltages(data: &ProbeData, times: &[Ns]) -> Vec<f64> {
    match data {
        ProbeData::Cap(segs) => {
            let mut i = 0;
            times
                .iter()
                .map(|&t| {
                    while i + 1 < segs.len() && segs[i + 1].t <= t {
                        i += 1;
                    }
                    segs.get(i).map_or(0.0, |s| s.at(t))
                })
                .collect()
        }
        ProbeData::Out {
            from,
            window,
            high_v,
        } => {
            // A first-order slew toward the driven level, stepped exactly
            // from edge to edge — a pulse shorter than the slew never
            // reaches the rail, whichever sample rate looks at it.
            let target = |level: bool| if level { *high_v } else { 0.0 };
            let mut level = window.level;
            let mut v = target(level);
            let mut t_prev = *from;
            let mut idx = 0;
            let step =
                |v: f64, tgt: f64, dt: Ns| tgt + (v - tgt) * (-(dt as f64) / OSC_OUT_TAU_NS).exp();
            times
                .iter()
                .map(|&t| {
                    while idx < window.edges.len() && window.edges[idx] <= t {
                        let te = window.edges[idx];
                        v = step(v, target(level), te.saturating_sub(t_prev));
                        t_prev = te;
                        level = !level;
                        idx += 1;
                    }
                    v = step(v, target(level), t.saturating_sub(t_prev));
                    t_prev = t;
                    v
                })
                .collect()
        }
    }
}

#[async_trait]
impl Instrument for SimScope {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn capabilities(&self) -> &[Capability] {
        &self.capabilities
    }

    fn label(&self) -> Option<String> {
        Some(SCOPE_LABEL.to_string())
    }

    async fn prepare(&mut self) -> Result<()> {
        tokio::time::sleep(Duration::from_millis(30)).await;
        self.ready = true;
        Ok(())
    }

    async fn configure(&mut self, cfg: AcqConfig) -> Result<EffectiveAcq> {
        if !self.ready {
            return Err(CoreError::Device("prepare() must complete first".into()));
        }
        if !self.bench.port_power(PORT_SCOPE) {
            return Err(CoreError::Device(format!("{SCOPE_LABEL} has no power")));
        }
        if !SCOPE_RATES.contains(&cfg.rate_hz) {
            return Err(CoreError::Config(format!(
                "rate {} Hz is not supported by {SCOPE_LABEL} (supported: {SCOPE_RATES:?})",
                cfg.rate_hz
            )));
        }
        if cfg.channels.is_empty() || cfg.channels.iter().any(|&c| c >= 2) {
            return Err(CoreError::Config(format!(
                "channel selection {:?} invalid for a 2-channel device",
                cfg.channels
            )));
        }
        if !cfg.continuous && cfg.samples == 0 {
            return Err(CoreError::Config("samples must be > 0".into()));
        }
        if !cfg.ranges_v.is_empty() {
            if cfg.ranges_v.len() != cfg.channels.len() {
                return Err(CoreError::Config(format!(
                    "{} ranges given for {} channels",
                    cfg.ranges_v.len(),
                    cfg.channels.len()
                )));
            }
            if let Some(bad) = cfg.ranges_v.iter().find(|r| !SCOPE_RANGES.contains(r)) {
                return Err(CoreError::Config(format!(
                    "range ±{bad} V is not supported by {SCOPE_LABEL} (supported: {SCOPE_RANGES:?})"
                )));
            }
            for (&ch, &r) in cfg.channels.iter().zip(&cfg.ranges_v) {
                self.ranges[usize::from(ch)] = r;
            }
        }
        let mut effective = cfg;
        effective.ranges_v = effective
            .channels
            .iter()
            .map(|&c| self.ranges[usize::from(c)])
            .collect();
        let calibration = effective
            .channels
            .iter()
            .zip(&effective.ranges_v)
            .map(|(&channel, &range_v)| {
                let (scale, offset) = scale_offset(range_v);
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
        let cfg = self
            .acq
            .clone()
            .ok_or_else(|| CoreError::Config("configure() must complete first".into()))?;
        let rate = cfg.rate_hz;
        let probes: Vec<(u8, ScopeProbe, f32)> = cfg
            .channels
            .iter()
            .zip(&cfg.ranges_v)
            .map(|(&c, &r)| (c, self.bench.wiring().scope[usize::from(c)], r))
            .collect();
        let seed = self.bench.seed();
        let n_ch = probes.len() as u8;
        let cancel = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(stream(
            self.bench.clone(),
            PORT_SCOPE,
            sink,
            cancel.clone(),
            (
                Encoding::U8Interleaved,
                n_ch,
                cfg.samples,
                rate,
                cfg.continuous,
            ),
            move |bench, start, first, count, buf| {
                let times: Vec<Ns> = (first..first + count)
                    .map(|n| sample_time(start, n, rate))
                    .collect();
                let (t0, t1) = (times[0], times[times.len() - 1]);
                // Ten slew time constants back: what came before is settled.
                let lead = (10.0 * OSC_OUT_TAU_NS) as Ns;
                let data = bench.read_at(t1, |w| {
                    w.port_power(PORT_SCOPE).then(|| {
                        probes
                            .iter()
                            .map(|&(_, probe, _)| match probe {
                                ScopeProbe::Cap => ProbeData::Cap(w.cap_segments(t0, t1)),
                                ScopeProbe::Out => {
                                    let from = t0.saturating_sub(lead);
                                    ProbeData::Out {
                                        from,
                                        window: w.digital_window(Net::OscOut, from, t1),
                                        high_v: w.osc_out_high_v(),
                                    }
                                }
                            })
                            .collect::<Vec<_>>()
                    })
                });
                let Some(data) = data else {
                    return false;
                };
                let volts: Vec<Vec<f64>> = data.iter().map(|d| probe_voltages(d, &times)).collect();
                for (i, &t) in times.iter().enumerate() {
                    for (k, &(ch, _, range)) in probes.iter().enumerate() {
                        let v = volts[k][i] + SCOPE_NOISE_V * noise(seed, ch, t);
                        buf.push(quantize(v, range));
                    }
                }
                true
            },
        ));
        self.cancel = Some(cancel);
        self.task = Some(task);
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
