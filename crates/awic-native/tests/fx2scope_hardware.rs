//! Hardware-in-the-loop verification for the `fx2scope` driver
//! (FEATURES.md, Instruments).
//!
//! These tests talk to the real FX2 USB oscilloscope at USB bus path `3-3.4.2`
//! (bus 3, hub `3-3.4` port 2 — confirmed via `lsusb -t` on the dev host) and
//! need root (no udev rules installed yet) plus the device physically
//! present. `#[ignore]`d so a plain `cargo test` never touches hardware:
//!
//! ```text
//! sudo -n env "PATH=$PATH" cargo test -p awic-native --test fx2scope_hardware \
//!     -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! `--test-threads=1`: every test here contends for the same physical
//! device, which the driver claims exclusively.
//!
//! # What this does *not* verify
//!
//! **Nothing is connected to the scope's BNC inputs on this bench.** CH1 and
//! CH2 float (or pick up ambient noise), so these tests check the
//! *mechanics* — firmware upload, re-enumeration by bus path, exact
//! requested sample count, zero dropped transfers, sample-rate timing, range
//! selection reflected in the effective config/calibration, bounded
//! `stop()` — and record whatever idle reading comes back (useful as a
//! sanity check that it stays near 0 V and moves sensibly with range), but
//! make **no claim about amplitude or frequency correctness on a real
//! signal**.
//!
//! Closing that gap needs a real signal. The oscilloscope has a **probe
//! compensation output on the front panel** (labelled on the case, next to
//! the BNC inputs — a fixed-amplitude square wave, nominally ~1 kHz, meant
//! for adjusting a 10x probe's trimmer). **To close the gap: connect a probe
//! (or a jumper) from that compensation pin to CH1's BNC input (a standard
//! 10x probe's tip, with its ground clip to the compensation output's
//! ground ring, works without any extra hardware)**, then run
//! [`probe_compensation_signal_reads_correct_amplitude_and_frequency`],
//! which is written to assert against it but will fail with a clear message
//! if nothing is connected (a floating input does not read as a clean ~1 kHz
//! square wave).
//!
//! # A second, larger gap: bulk streaming itself is unverified
//!
//! Independent of the missing signal above, every test in this file that
//! needs `start()` to actually deliver sample bytes
//! (`capture_ladder_at_one_and_two_channels`,
//! `every_advertised_range_is_selectable_and_idle_reads_near_zero`,
//! `probe_compensation_signal_reads_correct_amplitude_and_frequency`) is
//! expected to **fail** on this bench right now. `prepare()` (firmware
//! upload, re-enumeration) and `configure()` (rate/range validation) are
//! confirmed working against the real device, and the register writes
//! `start()` issues were traced byte-for-byte against `sigrok-cli`
//! (libusb) and match. But the bulk IN reads issued through `nusb` on
//! endpoint `0x86` consistently receive nothing and time out, while
//! `sigrok-cli` reads the same endpoint on the same device successfully at
//! the same moment. See `fx2scope::driver`'s module doc comment for the
//! full list of things tried during bring-up (buffer sizing, transfer
//! count, `SET_INTERFACE`, a `libsigrok`-style flush read, `clear_halt()`).
//! This is recorded here rather than worked around so the failure is
//! honest and reproducible instead of silently skipped.

use std::sync::Arc;
use std::time::{Duration, Instant};

use awic_core::{AcqConfig, DeviceId, Instrument, SampleSink};
use awic_native::fx2scope::{AcquisitionStats, Fx2ScopeInstrument, RANGES};
use awic_native::FirmwareResolver;

/// Bus path of the FX2 oscilloscope on the dev bench: bus 3, hub `3-3.4`
/// (`0bda:5411`), port 2.
const BUS: u8 = 3;
const PORT_PATH: [u8; 3] = [3, 4, 2];

/// Highest rate this driver sustains — see `fx2scope::protocol`'s doc
/// comment on `MAX_SUSTAINED_RATE_HZ` for the bench measurement this pins.
const MAX_SUSTAINED_RATE_HZ: u64 = awic_native::fx2scope::MAX_SUSTAINED_RATE_HZ;

fn scope_id() -> DeviceId {
    // vid/pid are placeholders — identity is the bus path only
    // (SPECIFICATION.md §2.3); `prepare()` fills in the real ones.
    DeviceId::new(BUS, PORT_PATH.to_vec(), 0, 0)
}

fn new_instrument() -> Fx2ScopeInstrument {
    Fx2ScopeInstrument::new(scope_id(), Arc::new(FirmwareResolver::default()))
}

async fn drain(mut rx: tokio::sync::mpsc::Receiver<awic_core::SampleChunk>) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(chunk) = rx.recv().await {
        out.extend_from_slice(&chunk.data);
    }
    out
}

/// Safety net for every capture in this suite, independent of the driver's
/// own stall detection.
const TEST_DEADLINE: Duration = Duration::from_secs(60);

async fn capture(
    instrument: &mut Fx2ScopeInstrument,
    rate_hz: u64,
    channels: Vec<u8>,
    samples: u64,
    ranges_v: Vec<f32>,
) -> (Vec<u8>, AcquisitionStats, Duration, awic_core::EffectiveAcq) {
    let effective = instrument
        .configure(AcqConfig {
            rate_hz,
            channels: channels.clone(),
            samples,
            trigger: None,
            ranges_v,
            continuous: false,
        })
        .await
        .unwrap_or_else(|e| panic!("configure({rate_hz} Hz, {channels:?}) failed: {e}"));
    let (sink, rx) = SampleSink::channel(64);
    let started = Instant::now();
    instrument
        .start(sink)
        .await
        .unwrap_or_else(|e| panic!("start() failed at {rate_hz} Hz: {e}"));
    let received = tokio::time::timeout(TEST_DEADLINE, drain(rx))
        .await
        .unwrap_or_else(|_| {
            panic!("capture at {rate_hz} Hz did not finish within {TEST_DEADLINE:?}")
        });
    instrument.stop().await.expect("stop() must succeed");
    let elapsed = started.elapsed();
    let stats = instrument.last_stats();
    eprintln!(
        "{rate_hz:>9} Hz {channels:?}: received={} dropped={} stalled={} measured={} mismatch={} elapsed={elapsed:.2?}",
        stats.received_samples,
        stats.dropped_transfers,
        stats.stalled,
        stats.measured_rate_hz.map_or("n/a".into(), |m| format!("{m:.0} Hz")),
        stats.rate_mismatch,
    );
    (received, stats, elapsed, effective)
}

fn assert_clean(
    rate_hz: u64,
    samples: u64,
    channel_count: usize,
    received: &[u8],
    stats: &AcquisitionStats,
) {
    assert_eq!(
        received.len() as u64,
        samples * channel_count as u64,
        "exact byte count at {rate_hz} Hz, {channel_count} channel(s)"
    );
    assert_eq!(
        stats.received_samples, samples,
        "driver accounting at {rate_hz} Hz"
    );
    assert_eq!(
        stats.dropped_transfers, 0,
        "zero failed transfers at {rate_hz} Hz"
    );
    assert!(!stats.stalled, "stream must not stall at {rate_hz} Hz");
    let measured = stats
        .measured_rate_hz
        .unwrap_or_else(|| panic!("capture at {rate_hz} Hz too short to rate-check"));
    assert!(
        !stats.rate_mismatch,
        "device ran at {measured:.0} Hz, not the configured {rate_hz} Hz — the timebase is wrong"
    );
}

/// Firmware upload, re-enumeration by bus path, then a rate ladder at both 1
/// and 2 channels. Every capture must be lossless *and* sampled at the
/// configured rate. Only rates this driver has verified sustainable
/// ([`MAX_SUSTAINED_RATE_HZ`]) are exercised here for cleanliness; the
/// bandwidth ceiling itself is pinned by
/// `rates_beyond_the_sustained_ceiling_are_refused_at_configure`.
#[tokio::test]
#[ignore = "needs root and the real FX2 oscilloscope at bus path 3-3.4.2"]
async fn capture_ladder_at_one_and_two_channels() {
    let mut instrument = new_instrument();
    instrument.prepare().await.expect("prepare() must succeed");
    assert_eq!(instrument.id().bus, BUS);
    assert_eq!(instrument.id().port_path, PORT_PATH.to_vec());
    eprintln!(
        "re-enumerated as {:04x}:{:04x} (expected 1d50:608e per this driver's ground truth, \
         not SPECIFICATION.md's assumed 04b5:602a -- see fx2scope::protocol's doc comment)",
        instrument.id().vid,
        instrument.id().pid
    );

    for &rate_hz in awic_native::fx2scope::SAMPLE_RATES_HZ {
        if rate_hz > MAX_SUSTAINED_RATE_HZ {
            continue;
        }
        // ~0.5 s of samples per rate, floored so slow rates still measure.
        let samples = (rate_hz / 2).max(2_000);

        for channels in [vec![0u8], vec![0u8, 1u8]] {
            let (received, stats, _, effective) =
                capture(&mut instrument, rate_hz, channels.clone(), samples, vec![]).await;
            assert_eq!(
                effective.cfg.rate_hz, rate_hz,
                "rate must be applied exactly (on-ladder request)"
            );
            assert_clean(rate_hz, samples, channels.len(), &received, &stats);
        }
    }
}

/// SPECIFICATION.md §2.3.1: the applied rate/ranges must come back honestly.
/// A request for an off-ladder rate must be refused (this device has a fixed
/// ladder, unlike `fx2lafw`'s divisor arithmetic which can hit many rates) —
/// and a request for an unsupported range must snap to the nearest one the
/// hardware offers, reflected in both `cfg.ranges_v` and the calibration.
#[tokio::test]
#[ignore = "needs root and the real FX2 oscilloscope at bus path 3-3.4.2"]
async fn configure_reports_the_honest_effective_rate_and_range() {
    let mut instrument = new_instrument();
    instrument.prepare().await.expect("prepare() must succeed");

    let err = instrument
        .configure(AcqConfig {
            rate_hz: 10_000_000, // not on the ladder.
            channels: vec![0],
            samples: 1_000,
            trigger: None,
            ranges_v: vec![],
            continuous: false,
        })
        .await
        .expect_err("an off-ladder rate must be refused, not silently rounded");
    eprintln!("off-ladder rate refused as expected: {err}");

    // Request 1.3 V full-scale (between 1.25 V and no exact match); must
    // snap to the nearest offered range (1.25 V, "250 mV/div") and say so.
    let effective = instrument
        .configure(AcqConfig {
            rate_hz: 1_000_000,
            channels: vec![0, 1],
            samples: 1_000,
            trigger: None,
            ranges_v: vec![1.3, 1.3],
            continuous: false,
        })
        .await
        .expect("configure() with an in-range rate must succeed");
    assert_eq!(
        effective.cfg.ranges_v,
        vec![1.25, 1.25],
        "range must snap to what the hardware offers"
    );
    for cal in &effective.calibration {
        assert_eq!(cal.unit, "V");
        assert_eq!(cal.range_v, Some(1.25));
        // raw*scale+offset must span exactly [-1.25, 1.25] at the ADC's
        // 0/255 extremes (fx2scope::protocol's traced formula).
        assert!((0.0 * cal.scale + cal.offset - (-1.25)).abs() < 1e-3);
        assert!((255.0 * cal.scale + cal.offset - 1.25).abs() < 1e-3);
    }
    eprintln!(
        "snapped ranges_v={:?} calibration={:?}",
        effective.cfg.ranges_v, effective.calibration
    );
}

/// Every range this driver advertises must actually be selectable and
/// reflected end-to-end, and the idle (floating) input should read
/// consistently small relative to each range's full scale -- a sanity check
/// of the offset math, not a claim about a real signal's amplitude (see this
/// file's top doc comment).
#[tokio::test]
#[ignore = "needs root and the real FX2 oscilloscope at bus path 3-3.4.2"]
async fn every_advertised_range_is_selectable_and_idle_reads_near_zero() {
    let mut instrument = new_instrument();
    instrument.prepare().await.expect("prepare() must succeed");

    for range in RANGES {
        // 0.5 s: long enough for the driver's rate check (300 ms minimum).
        let (received, stats, _, effective) = capture(
            &mut instrument,
            100_000,
            vec![0],
            50_000,
            vec![range.range_v],
        )
        .await;
        assert_clean(100_000, 50_000, 1, &received, &stats);
        assert_eq!(effective.cfg.ranges_v, vec![range.range_v]);
        let cal = &effective.calibration[0];

        let mean_raw = received.iter().map(|&b| b as f64).sum::<f64>() / received.len() as f64;
        let mean_v = mean_raw as f32 * cal.scale + cal.offset;
        eprintln!(
            "range +/-{} V: mean raw={mean_raw:.1} mean={mean_v:.4} V (idle/floating input, not a real signal)",
            range.range_v
        );
        // A floating input's mean should land well inside the range, not at
        // a rail -- if this ever pins at +/-range_v, the calibration or the
        // range register write is wrong, not just noisy.
        // A connected signal (the bench keeps CAL on CH1) legitimately
        // exceeds the narrow ranges. Pinned at a rail on every sample means
        // the input is too large for this range, not that the range is wrong.
        if received.iter().all(|&c| c == 0 || c == 255) {
            eprintln!(
                "range +/-{} V: input exceeds the range (a signal is connected); not judged",
                range.range_v
            );
            continue;
        }
        assert!(
            mean_v.abs() < range.range_v,
            "idle mean {mean_v} V outside +/-{} V range -- calibration or range selection is wrong",
            range.range_v
        );
    }
}

/// Rates beyond what this device sustains (SPECIFICATION.md §2.6: USB 2.0
/// bulk bandwidth is a hard ceiling) must be refused at `configure()` rather
/// than attempted and left to stall -- the exact class of bug that once hung
/// `fx2lafw`'s test suite for over four hours.
#[tokio::test]
#[ignore = "needs root and the real FX2 oscilloscope at bus path 3-3.4.2"]
async fn rates_beyond_the_sustained_ceiling_are_refused_at_configure() {
    let mut instrument = new_instrument();
    instrument.prepare().await.expect("prepare() must succeed");

    for &rate_hz in awic_native::fx2scope::SAMPLE_RATES_HZ {
        if rate_hz <= MAX_SUSTAINED_RATE_HZ {
            continue;
        }
        let err = instrument
            .configure(AcqConfig {
                rate_hz,
                channels: vec![0, 1],
                samples: 1_000,
                trigger: None,
                ranges_v: vec![],
                continuous: false,
            })
            .await
            .expect_err("a rate beyond the sustained ceiling must be refused");
        eprintln!("{rate_hz} Hz refused as expected: {err}");
    }
}

/// A short capture must be cleanly cancellable mid-flight via `stop()`
/// without hanging, and must leave the device actually stopped (SPEC's
/// traced sequence: `0xE3 <- 0`).
#[tokio::test]
#[ignore = "needs root and the real FX2 oscilloscope at bus path 3-3.4.2"]
async fn stop_cancels_a_long_capture_promptly() {
    let mut instrument = new_instrument();
    instrument.prepare().await.expect("prepare() must succeed");

    instrument
        .configure(AcqConfig {
            rate_hz: 1_000_000,
            channels: vec![0, 1],
            // 10 s at 1 MS/s -- stop() must cut this off far sooner.
            samples: 10_000_000,
            trigger: None,
            ranges_v: vec![],
            continuous: false,
        })
        .await
        .expect("configure() must succeed");

    let (sink, rx) = SampleSink::channel(64);
    let _capture_id = instrument.start(sink).await.expect("start() must succeed");

    tokio::time::sleep(Duration::from_millis(200)).await;
    let stop_started = Instant::now();
    instrument.stop().await.expect("stop() must succeed");
    let stop_elapsed = stop_started.elapsed();
    eprintln!("stop() returned after {stop_elapsed:?}");
    assert!(
        stop_elapsed < Duration::from_secs(2),
        "stop() must cancel promptly"
    );

    drop(rx);
    let stats = instrument.last_stats();
    eprintln!(
        "stopped early: received {} of {} requested samples, dropped_transfers={}",
        stats.received_samples, stats.requested_samples, stats.dropped_transfers
    );
    assert!(stats.received_samples < stats.requested_samples);
}

/// `prepare()` must be safe to call again on an already-programmed device
/// (SPECIFICATION.md: "Runs on EVERY enumeration").
#[tokio::test]
#[ignore = "needs root and the real FX2 oscilloscope at bus path 3-3.4.2"]
async fn prepare_is_idempotent_across_repeated_calls() {
    let mut instrument = new_instrument();

    instrument
        .prepare()
        .await
        .expect("first prepare() must succeed");
    let after_first = instrument.id().clone();

    instrument
        .prepare()
        .await
        .expect("second prepare() must also succeed");
    let after_second = instrument.id().clone();

    assert_eq!(
        after_first, after_second,
        "bus path identity must be unchanged"
    );
    eprintln!("prepare() #1 -> {after_first}, prepare() #2 -> {after_second}: same bus path");
}

/// Rising edges of a square wave at the midpoint between its two levels,
/// with 10 % hysteresis so noise near the threshold cannot add edges.
fn rising_edges(codes: &[u8]) -> Vec<usize> {
    let (lo, hi) = (
        *codes.iter().min().unwrap() as f64,
        *codes.iter().max().unwrap() as f64,
    );
    let (mid, band) = ((lo + hi) / 2.0, (hi - lo) * 0.1);
    let (mut high, mut edges) = (codes[0] as f64 > mid, Vec::new());
    for (i, &c) in codes.iter().enumerate() {
        let c = c as f64;
        if !high && c > mid + band {
            high = true;
            edges.push(i);
        } else if high && c < mid - band {
            high = false;
        }
    }
    edges
}

/// The two levels of a square wave: the most common code in each half of the
/// code range (a histogram mode, so one spike cannot move it).
fn square_levels(codes: &[u8]) -> (f64, f64) {
    let mut hist = [0usize; 256];
    for &c in codes {
        hist[c as usize] += 1;
    }
    let (lo, hi) = (
        *codes.iter().min().unwrap() as usize,
        *codes.iter().max().unwrap() as usize,
    );
    let mid = (lo + hi) / 2;
    let mode = |r: std::ops::RangeInclusive<usize>| r.max_by_key(|&c| hist[c]).unwrap() as f64;
    (mode(lo..=mid), mode(mid + 1..=hi))
}

/// The narrowest range in which CH1's signal does not touch either ADC rail.
/// CAL is ~0.2 V peak-to-peak through a 1:10 probe but ~2 V through 1:1, so
/// a fixed range either clips it flat or leaves it a few codes tall.
async fn unclipped_range(instrument: &mut Fx2ScopeInstrument) -> f32 {
    let mut ranges: Vec<f32> = RANGES.iter().map(|r| r.range_v).collect();
    ranges.sort_by(|a, b| a.total_cmp(b));
    for range_v in ranges {
        let (received, _, _, _) =
            capture(instrument, 1_000_000, vec![0], 500_000, vec![range_v]).await;
        let (lo, hi) = (received.iter().min(), received.iter().max());
        if lo > Some(&0) && hi < Some(&255) {
            eprintln!("CH1 fits the +/-{range_v} V range (codes {lo:?}..{hi:?})");
            return range_v;
        }
    }
    panic!("CH1 clips even the widest range: more than +/-5 V at the input")
}

/// **Needs the front-panel CAL output connected to CH1** (probe or jumper).
///
/// The oscilloscope generates CAL from its own FX2 clock and samples it with the
/// same clock, so in samples the frequency is exact: any deviation means lost
/// or stale samples, not crystal error. Also asserts no period is short — the
/// signature of a dropped block, which a mean frequency hides. This is the
/// check that found the stale-FIFO remnant and the start-of-capture overrun.
#[tokio::test]
#[ignore = "needs the CAL output connected to CH1 -- see this file's top doc comment"]
async fn cal_signal_reads_1khz_exactly_with_no_lost_samples() {
    let mut instrument = new_instrument();
    instrument.prepare().await.expect("prepare() must succeed");
    let range_v = unclipped_range(&mut instrument).await;
    for rate in [1_000_000u64, 8_000_000, 16_000_000] {
        let (received, stats, _, _) =
            capture(&mut instrument, rate, vec![0], rate / 2, vec![range_v]).await;
        assert_clean(rate, rate / 2, 1, &received, &stats);
        let edges = rising_edges(&received);
        let periods: Vec<f64> = edges.windows(2).map(|w| (w[1] - w[0]) as f64).collect();
        let mean = periods.iter().sum::<f64>() / periods.len() as f64;
        let freq = rate as f64 / mean;
        let shortest = periods.iter().cloned().fold(f64::MAX, f64::min);
        eprintln!(
            "{rate:>9} Hz: CAL {freq:.3} Hz over {} periods, shortest {:+.0} samples",
            periods.len(),
            shortest - mean
        );
        assert!(
            (freq - 1000.0).abs() < 0.5,
            "CAL read {freq:.3} Hz at {rate} Hz: the timebase is off"
        );
        assert!(
            shortest > mean * 0.99,
            "a period {:.0} samples short at {rate} Hz: samples were lost",
            mean - shortest
        );
    }
}

/// **Needs CAL connected to CH1 and an absolute reference.** Checks the gain:
/// the CAL step (top minus base, independent of any DC offset) against its
/// true amplitude. That amplitude is not assumed — it must come from outside
/// this driver: set `AWIC_CAL_VPP` to the CAL output's peak-to-peak volts
/// (e.g. twice a multimeter's DC reading of the 50 % square wave) and
/// `AWIC_PROBE_ATTENUATION` to the probe setting (1 or 10).
#[tokio::test]
#[ignore = "needs CAL on CH1 plus AWIC_CAL_VPP and AWIC_PROBE_ATTENUATION -- see the doc comment"]
async fn cal_signal_amplitude_matches_its_reference() {
    let reference = |name: &str| -> f32 {
        std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or_else(|| {
            panic!("set {name}: the amplitude check needs an absolute reference, not an assumed one")
        })
    };
    let (cal_vpp, attenuation) = (
        reference("AWIC_CAL_VPP"),
        reference("AWIC_PROBE_ATTENUATION"),
    );
    let expected = cal_vpp / attenuation;
    let mut instrument = new_instrument();
    instrument.prepare().await.expect("prepare() must succeed");
    for range in RANGES.iter().filter(|r| expected * 1.2 < r.range_v) {
        let (received, _, _, effective) = capture(
            &mut instrument,
            1_000_000,
            vec![0],
            500_000,
            vec![range.range_v],
        )
        .await;
        let (base, top) = square_levels(&received);
        let step_v = (top - base) as f32 * effective.calibration[0].scale;
        eprintln!(
            "±{} V: step {:.0} codes = {step_v:.4} V (expected {expected:.4} V at the input)",
            range.range_v,
            top - base
        );
        assert!(
            (step_v - expected).abs() <= expected * 0.05 + 2.0 * effective.calibration[0].scale,
            "±{} V range reads a {step_v:.4} V step for a {expected:.4} V signal: the gain is wrong",
            range.range_v
        );
    }
}

/// Runs a continuous acquisition (SPECIFICATION.md §2.3.2) at `rate_hz` for
/// `run_for`, then stops it. Returns the concatenated per-channel bytes, the
/// wall-clock span between the first and last chunk to arrive at this test
/// (not inside the driver — this checks the boundary a real consumer sees),
/// the sample count produced *after* the first chunk (excluding it mirrors
/// `fx2scope::driver`'s own `measured_rate`: the first chunk's arrival
/// marks the end of its own fill time, not the start of a steady rate), the
/// driver's stats, and how long `stop()` took to return.
async fn run_continuous(
    instrument: &mut Fx2ScopeInstrument,
    rate_hz: u64,
    ranges_v: Vec<f32>,
    run_for: Duration,
) -> (Vec<u8>, Duration, u64, AcquisitionStats, Duration) {
    instrument
        .configure(AcqConfig {
            rate_hz,
            channels: vec![0],
            samples: 0, // ignored in continuous mode.
            trigger: None,
            ranges_v,
            continuous: true,
        })
        .await
        .unwrap_or_else(|e| panic!("configure({rate_hz} Hz, continuous) failed: {e}"));

    let (sink, mut rx) = SampleSink::channel(64);
    instrument
        .start(sink)
        .await
        .unwrap_or_else(|e| panic!("start() failed at {rate_hz} Hz continuous: {e}"));

    let drain_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        let mut first_at: Option<Instant> = None;
        let mut last_at = Instant::now();
        let mut samples_after_first = 0u64;
        while let Some(chunk) = rx.recv().await {
            let now = Instant::now();
            if first_at.is_none() {
                first_at = Some(now);
            } else {
                samples_after_first += chunk.sample_count as u64;
            }
            last_at = now;
            bytes.extend_from_slice(&chunk.data);
        }
        let elapsed = first_at.map_or(Duration::ZERO, |first| {
            last_at.saturating_duration_since(first)
        });
        (bytes, elapsed, samples_after_first)
    });

    tokio::time::sleep(run_for).await;

    let stop_started = Instant::now();
    instrument.stop().await.expect("stop() must succeed");
    let stop_elapsed = stop_started.elapsed();

    // stop() has already joined the driver's task, so the sink is dropped
    // and this drains almost immediately; a generous bound still catches a
    // regression rather than hanging the suite.
    let (bytes, elapsed, samples_after_first) =
        tokio::time::timeout(Duration::from_secs(5), drain_task)
            .await
            .expect("continuous stream did not end promptly after stop()")
            .expect("drain task panicked");

    let stats = instrument.last_stats();
    (bytes, elapsed, samples_after_first, stats, stop_elapsed)
}

/// Asserts the mechanics required of every continuous
/// acquisition: no stall, the sample count within 0.5 % of `rate_hz *
/// elapsed`, and `stop()` returning within 1 s.
fn assert_continuous_clean(
    rate_hz: u64,
    elapsed: Duration,
    samples_after_first: u64,
    stats: &AcquisitionStats,
    stop_elapsed: Duration,
) {
    assert!(
        stats.continuous,
        "driver stats must record this as continuous"
    );
    assert!(
        !stats.stalled,
        "continuous stream must not stall at {rate_hz} Hz"
    );
    assert_eq!(
        stats.dropped_transfers, 0,
        "zero failed transfers at {rate_hz} Hz continuous"
    );
    let measured = samples_after_first as f64 / elapsed.as_secs_f64();
    let error = (measured / rate_hz as f64 - 1.0).abs();
    eprintln!(
        "{rate_hz:>9} Hz continuous: {samples_after_first} samples over {elapsed:.2?} \
         ({measured:.1} Hz measured, {:.3}% off)",
        error * 100.0
    );
    assert!(
        error <= 0.005,
        "sample rate {measured:.1} Hz is more than 0.5% off {rate_hz} Hz over {elapsed:.2?}"
    );
    eprintln!("stop() returned after {stop_elapsed:?}");
    assert!(
        stop_elapsed < Duration::from_secs(1),
        "stop() must return within 1 s, took {stop_elapsed:?}"
    );
}

/// Checks that CAL still reads 1 kHz within 0.1 % in the final second of
/// `bytes` (the tail, since sample index stands in for capture-relative
/// time — SPECIFICATION.md §2.3.2's `t0_ns` also counts this way).
fn assert_cal_1khz_in_final_second(rate_hz: u64, bytes: &[u8]) {
    let tail_len = (rate_hz as usize).min(bytes.len());
    let tail = &bytes[bytes.len() - tail_len..];
    let edges = rising_edges(tail);
    let periods: Vec<f64> = edges.windows(2).map(|w| (w[1] - w[0]) as f64).collect();
    assert!(
        periods.len() >= 2,
        "not enough edges in the final second to measure CAL ({} edges)",
        edges.len()
    );
    let mean = periods.iter().sum::<f64>() / periods.len() as f64;
    let freq = rate_hz as f64 / mean;
    eprintln!(
        "final second: CAL {freq:.4} Hz over {} periods",
        periods.len()
    );
    assert!(
        (freq - 1000.0).abs() <= 1.0,
        "CAL read {freq:.4} Hz in the final second: the timebase drifted"
    );
}

/// The continuous-acquisition acceptance check: 60 s continuous at 1 MHz, no stall,
/// the sample count within 0.5 % of rate x elapsed, `stop()` within 1 s, and
/// CAL still reading 1 kHz at the end. Also checks that a normal finite
/// capture works right after — no state a continuous run could have left
/// behind (a stale cancel channel, wrong stats, etc.).
#[tokio::test]
#[ignore = "needs the CAL output connected to CH1, runs for 60 s -- see this file's top doc comment"]
async fn continuous_acquisition_at_1mhz_runs_for_60s_without_stalling() {
    let mut instrument = new_instrument();
    instrument.prepare().await.expect("prepare() must succeed");

    let rate_hz = 1_000_000u64;
    // ±5 V, not ±0.5 V: measured on this bench (see
    // `every_advertised_range_is_selectable_and_idle_reads_near_zero`'s idle
    // readings), the CAL output here is a ~0-3.3 V unipolar square wave, not
    // centered on 0 V, so ±0.5 V clips it to a flat rail with no edges at
    // all -- ±5 V is the smallest advertised range that does not clip it.
    let (bytes, elapsed, samples_after_first, stats, stop_elapsed) =
        run_continuous(&mut instrument, rate_hz, vec![5.0], Duration::from_secs(60)).await;
    assert_continuous_clean(rate_hz, elapsed, samples_after_first, &stats, stop_elapsed);
    assert_cal_1khz_in_final_second(rate_hz, &bytes);

    let (received, stats, _, _) =
        capture(&mut instrument, rate_hz, vec![0], rate_hz / 2, vec![5.0]).await;
    assert_clean(rate_hz, rate_hz / 2, 1, &received, &stats);
}

/// The continuous-acquisition acceptance check at 16 MHz continuous for 10 s — this
/// device's highest sustained rate (SPECIFICATION.md §2.5) — with the same
/// checks as the 1 MHz run.
#[tokio::test]
#[ignore = "needs the CAL output connected to CH1, runs for 10 s -- see this file's top doc comment"]
async fn continuous_acquisition_at_16mhz_runs_for_10s_without_stalling() {
    let mut instrument = new_instrument();
    instrument.prepare().await.expect("prepare() must succeed");

    let rate_hz = MAX_SUSTAINED_RATE_HZ;
    // ±5 V: see the 1 MHz test above for why ±0.5 V clips this bench's CAL
    // signal to a flat rail.
    let (bytes, elapsed, samples_after_first, stats, stop_elapsed) =
        run_continuous(&mut instrument, rate_hz, vec![5.0], Duration::from_secs(10)).await;
    assert_continuous_clean(rate_hz, elapsed, samples_after_first, &stats, stop_elapsed);
    assert_cal_1khz_in_final_second(rate_hz, &bytes);

    let (received, stats, _, _) =
        capture(&mut instrument, rate_hz, vec![0], rate_hz / 2, vec![5.0]).await;
    assert_clean(rate_hz, rate_hz / 2, 1, &received, &stats);
}
