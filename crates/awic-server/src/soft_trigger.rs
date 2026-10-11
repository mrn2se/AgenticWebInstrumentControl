//! A software edge trigger, for instruments that cannot trigger themselves
//! (live test B-32).
//!
//! Neither native driver arms a hardware trigger: the oscilloscope and the fx2lafw
//! analyzer both just stream. Before this, a configured trigger was accepted,
//! echoed back in `cfg` and ignored, and the capture was filed as if its edge
//! sat in the middle. Now the server does what a scope does: the driver
//! streams continuously, [`Window`] keeps the last `samples / 2` sample
//! frames as pre-trigger history, scans the trigger channel for the edge, and
//! once it has `samples - samples / 2` frames from the edge onward hands the
//! window on to be filed — with `trigger_position` the sample the edge is
//! actually at. A trigger that never fires files nothing; [`TriggerWatch`]
//! says what the trigger channel did instead, so the caller can tell a level
//! outside the signal from a signal that never moved.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::sync::mpsc;

use awic_core::{ChannelCalibration, SampleChunk, Slope, Trigger};
use awic_proto::frame::Encoding;
use awic_proto::{AcqState, Event, PROTOCOL_VERSION};

use crate::state::{AppState, SharedInstrument};

/// Sample frames per chunk handed to the forwarder once the window is full:
/// small enough that a live view still gets frames of a sensible size.
const OUT_CHUNK_FRAMES: usize = 1 << 16;

/// What the trigger is waiting for, resolved against the effective
/// configuration once, at `acq.start`.
#[derive(Debug, Clone)]
pub struct EdgeSpec {
    /// The hardware channel, for messages (`CH2`, `D4`).
    pub channel: u8,
    pub slope: Slope,
    /// Volts for analog; ignored for digital, where an edge is a change of bit.
    pub level: f64,
    /// Where the trigger channel sits in each sample frame (analog), and its
    /// calibration.
    position: usize,
    scale: f64,
    offset: f64,
}

impl EdgeSpec {
    /// Resolves `trigger` against what the device will actually acquire.
    /// Errors name the channels that can be triggered on.
    pub fn resolve(
        trigger: &Trigger,
        channels: &[u8],
        calibration: &[ChannelCalibration],
    ) -> Result<Self, String> {
        let Trigger::Edge {
            channel,
            slope,
            level,
        } = trigger;
        let Some(position) = channels.iter().position(|c| c == channel) else {
            return Err(format!(
                "trigger channel {channel} is not acquired: the trigger has to be on one of \
                 the configured channels {channels:?}"
            ));
        };
        let cal = calibration.get(position);
        // A digital edge is a change of bit and needs no level (B-96); an
        // analog one does, and 0 V is no default to guess.
        let analog = cal.is_some_and(|c| !c.unit.is_empty());
        let level = match level {
            Some(level) => f64::from(*level),
            None if analog => {
                return Err(format!(
                    "an analog trigger needs `level` in volts: CH{} has no logic level to \
                     default to",
                    u16::from(*channel) + 1
                ))
            }
            None => 0.5,
        };
        Ok(Self {
            channel: *channel,
            slope: *slope,
            level,
            position,
            scale: cal.map_or(1.0, |c| f64::from(c.scale)),
            offset: cal.map_or(0.0, |c| f64::from(c.offset)),
        })
    }
}

/// What the trigger channel did while armed — shared with whoever is waiting
/// on the capture, so a trigger that never fires can be explained.
#[derive(Debug)]
pub struct TriggerWatch {
    pub spec: EdgeSpec,
    pub analog: AtomicBool,
    fired: AtomicBool,
    /// Lowest and highest value the trigger channel showed (volts, or 0/1).
    range: Mutex<Option<(f64, f64)>>,
}

impl TriggerWatch {
    pub fn new(spec: EdgeSpec) -> Arc<Self> {
        Arc::new(Self {
            spec,
            analog: AtomicBool::new(true),
            fired: AtomicBool::new(false),
            range: Mutex::new(None),
        })
    }

    pub fn fired(&self) -> bool {
        self.fired.load(Ordering::Acquire)
    }

    /// Why nothing has been captured yet, in one sentence an agent can act on.
    pub fn not_fired_reason(&self) -> String {
        let spec = &self.spec;
        let analog = self.analog.load(Ordering::Relaxed);
        let name = if analog {
            format!("CH{}", u16::from(spec.channel) + 1)
        } else {
            format!("D{}", spec.channel)
        };
        let slope = match spec.slope {
            Slope::Rising => "rising",
            Slope::Falling => "falling",
        };
        let edge = if analog {
            format!("a {slope} edge on {name} through {:.3} V", spec.level)
        } else {
            format!("a {slope} edge on {name}")
        };
        let seen = match *self.range.lock().unwrap() {
            None => "no samples arrived".to_string(),
            Some((lo, hi)) if analog => {
                let hint = if spec.level < lo || spec.level > hi {
                    " — the level is outside that, so it cannot fire"
                } else {
                    ""
                };
                format!("{name} stayed between {lo:.3} V and {hi:.3} V{hint}")
            }
            Some((lo, hi)) if lo == hi => {
                format!("{name} stayed {}", if hi > 0.5 { "high" } else { "low" })
            }
            Some(_) => {
                format!("{name} toggled, but not {slope} after the pre-trigger history filled")
            }
        };
        format!("trigger never fired: waited for {edge}; {seen}")
    }

    /// What to try next. A digital trigger has no level to move (B-107).
    pub fn advice(&self) -> &'static str {
        if self.analog.load(Ordering::Relaxed) {
            "Move the level inside the signal, raise timeout_s, or capture without a trigger"
        } else {
            "Check that the trigger channel is the wired one and that the signal runs while the \
             capture is armed, raise timeout_s, or capture without a trigger"
        }
    }

    fn observe(&self, lo: f64, hi: f64) {
        let mut range = self.range.lock().unwrap();
        *range = Some(match *range {
            None => (lo, hi),
            Some((a, b)) => (a.min(lo), b.max(hi)),
        });
    }
}

/// The trigger state machine over raw sample bytes: pure, so it can be tested
/// without a device.
pub struct Window {
    encoding: Encoding,
    channel_count: u8,
    frame_bytes: usize,
    pre: usize,
    post: usize,
    spec: EdgeSpec,
    /// Hysteresis, in the trigger channel's units: the signal must have been
    /// this far on the far side of the level before a crossing counts, so
    /// noise sitting on the level does not trigger.
    hysteresis: f64,
    /// Pre-trigger history, at most `pre` frames.
    history: VecDeque<u8>,
    /// Frames seen, which also gates arming until the history is full.
    seen: u64,
    armed: bool,
    /// The window from the trigger on, once fired.
    out: Option<Vec<u8>>,
    /// `t0_ns` of the first chunk and the rate, to stamp the window.
    t0_ns: Option<u64>,
    rate_hz: u64,
}

/// What [`Window::feed`] concluded.
#[derive(Debug, PartialEq)]
pub enum Feed {
    Waiting,
    /// The window is complete: `pre` frames of history, then the edge.
    Done {
        bytes: Vec<u8>,
        t0_ns: u64,
    },
}

impl Window {
    /// `samples` is the capture length asked for; the edge lands at
    /// `samples / 2`, the convention `trigger_position` already documents.
    pub fn new(
        spec: EdgeSpec,
        samples: u64,
        rate_hz: u64,
        encoding: Encoding,
        channel_count: u8,
    ) -> Self {
        let frame_bytes = match encoding {
            Encoding::PackedDigital => 1,
            Encoding::I16 => 2 * usize::from(channel_count.max(1)),
            _ => usize::from(channel_count.max(1)),
        };
        let pre = (samples / 2) as usize;
        let hysteresis = match encoding {
            Encoding::PackedDigital => 0.0,
            // Two codes: a step every real edge clears, and more than the
            // ±1 code a quiet input dithers by.
            _ => 2.0 * spec.scale.abs(),
        };
        Self {
            encoding,
            channel_count,
            frame_bytes,
            pre,
            post: samples as usize - pre,
            spec,
            hysteresis,
            history: VecDeque::with_capacity(pre * frame_bytes),
            seen: 0,
            armed: false,
            out: None,
            t0_ns: None,
            rate_hz: rate_hz.max(1),
        }
    }

    /// Where the edge is in the filed capture.
    pub fn trigger_position(&self) -> u64 {
        self.pre as u64
    }

    pub fn channel_count(&self) -> u8 {
        self.channel_count
    }

    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    fn value(&self, frame: &[u8]) -> f64 {
        match self.encoding {
            Encoding::PackedDigital => f64::from((frame[0] >> self.spec.channel) & 1),
            Encoding::I16 => {
                let at = 2 * self.spec.position;
                f64::from(i16::from_le_bytes([frame[at], frame[at + 1]])) * self.spec.scale
                    + self.spec.offset
            }
            _ => f64::from(frame[self.spec.position]) * self.spec.scale + self.spec.offset,
        }
    }

    /// The threshold: an analog level as configured, a digital edge at half.
    fn level(&self) -> f64 {
        match self.encoding {
            Encoding::PackedDigital => 0.5,
            _ => self.spec.level,
        }
    }

    /// Feeds one chunk's bytes, starting at `t0_ns`.
    pub fn feed(&mut self, data: &[u8], t0_ns: u64, watch: &TriggerWatch) -> Feed {
        self.t0_ns.get_or_insert(t0_ns);
        let level = self.level();
        let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
        let mut frames = data.chunks_exact(self.frame_bytes);
        if self.out.is_none() {
            for frame in frames.by_ref() {
                let v = self.value(frame);
                lo = lo.min(v);
                hi = hi.max(v);
                let full = self.history.len() >= self.pre * self.frame_bytes;
                let fire = full
                    && match self.spec.slope {
                        Slope::Rising => {
                            if v < level - self.hysteresis {
                                self.armed = true;
                            }
                            self.armed && v >= level
                        }
                        Slope::Falling => {
                            if v > level + self.hysteresis {
                                self.armed = true;
                            }
                            self.armed && v <= level
                        }
                    };
                if fire {
                    let mut out = Vec::with_capacity((self.pre + self.post) * self.frame_bytes);
                    out.extend(self.history.drain(..));
                    out.extend_from_slice(frame);
                    self.out = Some(out);
                    watch.fired.store(true, Ordering::Release);
                    break;
                }
                if !full {
                    // Arming starts only once there is history to keep:
                    // what the signal did before that does not count.
                    self.armed = false;
                } else {
                    self.history.drain(..self.frame_bytes);
                }
                self.history.extend(frame);
                self.seen += 1;
            }
            if lo <= hi {
                watch.observe(lo, hi);
            }
        }
        let Some(out) = self.out.as_mut() else {
            return Feed::Waiting;
        };
        let want = (self.pre + self.post) * self.frame_bytes;
        let rest = frames.remainder().len() + frames.len() * self.frame_bytes;
        let start = data.len() - rest;
        let take = (want - out.len()).min(rest);
        out.extend_from_slice(&data[start..start + take]);
        if out.len() < want {
            return Feed::Waiting;
        }
        // The window starts `pre` frames before the edge, and the edge is
        // frame `seen` of the stream.
        let first = self.seen - self.pre as u64;
        let t0 = self.t0_ns.unwrap_or(0) + first * 1_000_000_000 / self.rate_hz;
        Feed::Done {
            bytes: self.out.take().unwrap_or_default(),
            t0_ns: t0,
        }
    }
}

/// Runs the trigger between the driver's sink (`driver_rx`) and the
/// forwarder (`out`), then stops the instrument once the window is filed.
/// Dropping `out` is what tells the forwarder the capture is complete.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    state: AppState,
    stream: u16,
    instrument: SharedInstrument,
    mut driver_rx: mpsc::Receiver<SampleChunk>,
    out: mpsc::Sender<SampleChunk>,
    watch: Arc<TriggerWatch>,
    samples: u64,
    rate_hz: u64,
) {
    let mut window: Option<Window> = None;
    let mut done = None;
    while let Some(chunk) = driver_rx.recv().await {
        let w = window.get_or_insert_with(|| {
            watch
                .analog
                .store(chunk.encoding != Encoding::PackedDigital, Ordering::Relaxed);
            Window::new(
                watch.spec.clone(),
                samples,
                rate_hz,
                chunk.encoding,
                chunk.channel_count,
            )
        });
        if let Feed::Done { bytes, t0_ns } = w.feed(&chunk.data, chunk.t0_ns, &watch) {
            done = Some((bytes, t0_ns));
            break;
        }
    }
    let (Some((bytes, t0_ns)), Some(w)) = (done, window) else {
        // The stream ended (stopped, or the device gave up) before the
        // trigger fired: nothing to file.
        return;
    };
    state.event_bus().publish(Event::AcqState {
        v: PROTOCOL_VERSION,
        stream,
        state: AcqState::Running,
        msg: Some(format!("triggered at sample {}", w.trigger_position())),
    });
    // Stop the device before filing: it was streaming continuously.
    drop(driver_rx);
    if let Err(err) = instrument.lock().await.stop().await {
        tracing::warn!(stream, %err, "instrument.stop() after software trigger failed");
    }
    let frame_bytes = bytes.len() / samples.max(1) as usize;
    let ns_per_frame = 1_000_000_000 / rate_hz.max(1);
    for (i, piece) in bytes
        .chunks(OUT_CHUNK_FRAMES * frame_bytes.max(1))
        .enumerate()
    {
        let chunk = SampleChunk {
            encoding: w.encoding(),
            channel_count: w.channel_count(),
            sample_count: (piece.len() / frame_bytes.max(1)) as u32,
            t0_ns: t0_ns + (i * OUT_CHUNK_FRAMES) as u64 * ns_per_frame,
            data: Bytes::copy_from_slice(piece),
        };
        if out.send(chunk).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(slope: Slope, level: f32) -> EdgeSpec {
        let level = Some(level);
        let cal = ChannelCalibration {
            channel: 1,
            scale: 0.02,
            offset: -2.5,
            unit: "V".into(),
            range_v: Some(2.5),
        };
        EdgeSpec::resolve(
            &Trigger::Edge {
                channel: 1,
                slope,
                level,
            },
            &[0, 1],
            &[cal.clone(), cal],
        )
        .unwrap()
    }

    /// Two-channel u8 frames; CH2 a square wave between codes 135 (0.2 V)
    /// and 235 (2.2 V) with period `period`, CH1 a ramp that numbers the
    /// frame, so where the window starts can be read back.
    fn square(frames: std::ops::Range<usize>, period: usize) -> Vec<u8> {
        frames
            .flat_map(|i| {
                [
                    (i % 256) as u8,
                    if (i % period) < period / 2 { 135 } else { 235 },
                ]
            })
            .collect()
    }

    fn feed_all(w: &mut Window, data: &[u8], chunk: usize, watch: &TriggerWatch) -> Feed {
        for (i, c) in data.chunks(chunk * 2).enumerate() {
            match w.feed(c, (i * chunk) as u64 * 1000, watch) {
                Feed::Waiting => continue,
                done => return done,
            }
        }
        Feed::Waiting
    }

    #[test]
    fn a_rising_edge_lands_at_the_middle_of_the_window() {
        let watch = TriggerWatch::new(spec(Slope::Rising, 1.0));
        let mut w = Window::new(
            watch.spec.clone(),
            64,
            1_000_000,
            Encoding::U8Interleaved,
            2,
        );
        // Low for frames 0..50, high 50..100, ...: history fills at 32, the
        // first rising edge after that is frame 50.
        let Feed::Done { bytes, t0_ns } = feed_all(&mut w, &square(0..1000, 100), 7, &watch) else {
            panic!("should have fired")
        };
        assert!(watch.fired());
        assert_eq!(bytes.len(), 64 * 2);
        assert_eq!(w.trigger_position(), 32);
        // Frame 32 of the window is the edge: the first high sample...
        assert_eq!(bytes[32 * 2 + 1], 235);
        assert_eq!(bytes[31 * 2 + 1], 135);
        // ...and it is stream frame 50, so the window starts at frame 18.
        assert_eq!(bytes[0], 18);
        assert_eq!(t0_ns, 18_000);
    }

    #[test]
    fn a_falling_edge_is_found_too() {
        let watch = TriggerWatch::new(spec(Slope::Falling, 1.0));
        let mut w = Window::new(
            watch.spec.clone(),
            64,
            1_000_000,
            Encoding::U8Interleaved,
            2,
        );
        let Feed::Done { bytes, .. } = feed_all(&mut w, &square(0..1000, 100), 13, &watch) else {
            panic!("should have fired")
        };
        // First falling edge with history full is frame 100.
        assert_eq!(bytes[0], (100 - 32) as u8);
        assert_eq!(bytes[32 * 2 + 1], 135);
        assert_eq!(bytes[31 * 2 + 1], 235);
    }

    #[test]
    fn a_level_outside_the_signal_never_fires_and_says_why() {
        // Round 8: a trigger below the signal's minimum, which a real
        // trigger can never cross.
        let watch = TriggerWatch::new(spec(Slope::Rising, 0.0));
        let mut w = Window::new(
            watch.spec.clone(),
            64,
            1_000_000,
            Encoding::U8Interleaved,
            2,
        );
        assert_eq!(
            feed_all(&mut w, &square(0..5000, 100), 64, &watch),
            Feed::Waiting
        );
        assert!(!watch.fired());
        let why = watch.not_fired_reason();
        assert!(why.contains("rising edge on CH2 through 0.000 V"), "{why}");
        assert!(why.contains("between 0.200 V and 2.200 V"), "{why}");
        assert!(why.contains("outside"), "{why}");
        assert!(watch.advice().contains("Move the level"));
    }

    #[test]
    fn a_digital_trigger_that_never_fires_gives_no_level_advice() {
        // B-107: a digital trigger has no level, so "move the level" was
        // advice it could not follow.
        let watch = TriggerWatch::new(spec(Slope::Falling, 0.5));
        watch.analog.store(false, Ordering::Relaxed);
        watch.observe(1.0, 1.0);
        assert!(watch.not_fired_reason().contains("stayed high"));
        assert!(!watch.advice().contains("level"), "{}", watch.advice());
        assert!(watch.advice().contains("trigger channel"));
    }

    #[test]
    fn noise_on_the_level_does_not_trigger() {
        // Codes 174/175/176 = 0.98/1.0/1.02 V: dithering on a 1 V level.
        let watch = TriggerWatch::new(spec(Slope::Rising, 1.0));
        let mut w = Window::new(
            watch.spec.clone(),
            16,
            1_000_000,
            Encoding::U8Interleaved,
            2,
        );
        let noise: Vec<u8> = (0..2000).flat_map(|i| [0, 174 + (i % 3) as u8]).collect();
        assert_eq!(feed_all(&mut w, &noise, 50, &watch), Feed::Waiting);
    }

    #[test]
    fn a_digital_edge_is_a_change_of_the_channel_bit() {
        let spec = EdgeSpec::resolve(
            &Trigger::Edge {
                channel: 4,
                slope: Slope::Rising,
                level: None,
            },
            &[4, 6],
            &[
                ChannelCalibration::digital(4),
                ChannelCalibration::digital(6),
            ],
        )
        .unwrap();
        let watch = TriggerWatch::new(spec);
        let mut w = Window::new(
            watch.spec.clone(),
            10,
            1_000_000,
            Encoding::PackedDigital,
            2,
        );
        // D6 toggles constantly and must be ignored; D4 rises at frame 40.
        let data: Vec<u8> = (0..100)
            .map(|i| (u8::from(i >= 40) << 4) | (u8::from(i % 2 == 0) << 6))
            .collect();
        let Feed::Done { bytes, t0_ns } = w.feed(&data, 0, &watch) else {
            panic!("should have fired")
        };
        assert_eq!(bytes.len(), 10);
        assert_eq!(bytes[5] & 0x10, 0x10);
        assert_eq!(bytes[4] & 0x10, 0);
        assert_eq!(t0_ns, 35_000);
    }

    #[test]
    fn an_analog_trigger_without_a_level_is_refused() {
        let cal = ChannelCalibration {
            channel: 0,
            scale: 0.02,
            offset: -2.5,
            unit: "V".into(),
            range_v: Some(2.5),
        };
        let err = EdgeSpec::resolve(
            &Trigger::Edge {
                channel: 0,
                slope: Slope::Rising,
                level: None,
            },
            &[0],
            &[cal],
        )
        .unwrap_err();
        assert!(err.contains("needs `level`"), "{err}");
    }

    #[test]
    fn a_trigger_on_a_channel_not_acquired_is_refused() {
        let err = EdgeSpec::resolve(
            &Trigger::Edge {
                channel: 1,
                slope: Slope::Rising,
                level: Some(1.0),
            },
            &[0],
            &[],
        )
        .unwrap_err();
        assert!(err.contains("not acquired"), "{err}");
    }
}
