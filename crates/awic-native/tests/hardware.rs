//! Hardware-in-the-loop verification for the `fx2lafw` driver
//! (FEATURES.md, Instruments).
//!
//! These tests talk to the real fx2lafw logic analyzer at USB bus path
//! `3-3.4.1.1` and need root (full raw USB access without udev rules
//! installed) plus the device physically present. They are `#[ignore]`d so a
//! plain `cargo test` never touches hardware. Run them explicitly:
//!
//! ```text
//! sudo -n env "PATH=$PATH" cargo test -p awic-native --test hardware \
//!     -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! "The top rate" in these tests means `protocol::MAX_SUSTAINED_RATE_HZ`,
//! which came down from 24 MHz to 16 MHz in B-63: 24 MHz streams on an
//! otherwise quiet bus and stalled 12 times in 23 acquisitions on a shared
//! one, and `configure` refuses it now.
//!
//! `--test-threads=1` matters: every test here contends for the same
//! physical device, which the driver claims exclusively.
//!
//! # What this does *not* verify
//!
//! Most of these tests leave the logic analyzer's 8 probe inputs
//! unconnected — they float or read whatever idle state the input buffers
//! settle to — and check only the *mechanics* (firmware upload,
//! re-enumeration by bus path, exact requested sample count, zero dropped
//! transfers, sample-rate timing), recording whatever idle pattern comes
//! back for comparison with `sigrok-cli` but making no claim about decoding
//! a real signal. The `continuous_*` tests are the exception: they need D0
//! wired to a real ~1 kHz signal (the FX2 oscilloscope's front-panel CAL
//! output) so the timebase can be checked against
//! ground truth instead of just counted.

use std::sync::Arc;
use std::time::{Duration, Instant};

use awic_core::{AcqConfig, DeviceId, Instrument, SampleSink};
use awic_native::fx2lafw::{AcquisitionStats, Fx2lafwInstrument};
use awic_native::FirmwareResolver;

/// Bus path of the fx2lafw logic analyzer on the dev bench, per the task
/// description: bus 3, behind hub `3-3.4.1` port 1 (bench topology, checked
/// against `lsusb -t` at the time this constant was set).
const BUS: u8 = 3;
const PORT_PATH: [u8; 4] = [3, 4, 1, 1];

fn logic_id() -> DeviceId {
    // vid/pid are placeholders here — `DeviceId`'s identity is the bus path
    // only (SPECIFICATION.md §2.3), and `prepare()` fills in the real ones
    // once it has actually enumerated the device.
    DeviceId::new(BUS, PORT_PATH.to_vec(), 0, 0)
}

fn new_instrument() -> Fx2lafwInstrument {
    Fx2lafwInstrument::new(logic_id(), Arc::new(FirmwareResolver::default()))
}

/// Drains every chunk from `rx`, returning the concatenated packed-digital
/// bytes. Stops as soon as the channel closes (the driver's task finished).
async fn drain(mut rx: tokio::sync::mpsc::Receiver<awic_core::SampleChunk>) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(chunk) = rx.recv().await {
        out.extend_from_slice(&chunk.data);
    }
    out
}

/// Safety net for every capture in this suite, independent of the driver's
/// own stall detection: if the driver ever regresses, the test fails loudly.
const TEST_DEADLINE: Duration = Duration::from_secs(60);

/// Configures, captures and stops once, returning the sink's bytes, the
/// driver's stats and the wall-clock duration. Panics past [`TEST_DEADLINE`].
async fn capture(
    instrument: &mut Fx2lafwInstrument,
    rate_hz: u64,
    samples: u64,
) -> (Vec<u8>, AcquisitionStats, Duration) {
    instrument
        .configure(AcqConfig {
            rate_hz,
            channels: (0..8).collect(),
            samples,
            trigger: None,
            ranges_v: vec![],
            continuous: false,
        })
        .await
        .unwrap_or_else(|e| panic!("configure({rate_hz} Hz) failed: {e}"));
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
        "{rate_hz:>9} Hz: received={} dropped={} stalled={} primed={} reset_before={} measured={} mismatch={} elapsed={elapsed:.2?}",
        stats.received_samples,
        stats.dropped_transfers,
        stats.stalled,
        stats.primed,
        stats.reset_before,
        stats.measured_rate_hz.map_or("n/a".into(), |m| format!("{m:.0} Hz")),
        stats.rate_mismatch,
    );
    (received, stats, elapsed)
}

/// A capture that must be complete, lossless, and sampled at the rate asked.
fn assert_clean(rate_hz: u64, samples: u64, received: &[u8], stats: &AcquisitionStats) {
    assert_eq!(
        received.len() as u64,
        samples,
        "exact sample count at {rate_hz} Hz"
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

/// Firmware upload, re-enumeration by bus path, then a rate ladder chosen to
/// cross exactly the transitions where fx2lafw's rate-change lag was
/// observed (see `Fx2lafwInstrument::last_rate_hz`): 20 -> 25 kHz (both on
/// the 30 MHz clock) and 25 kHz -> 1 MHz (30 -> 48 MHz), then up to 24 MHz,
/// the highest rate 8 channels sustain over USB 2.0. Every capture must be
/// lossless *and* sampled at the configured rate.
#[tokio::test]
#[ignore = "needs root and the real fx2lafw logic analyzer at bus path 3-3.4.1.1"]
async fn capture_ladder_across_the_firmware_rate_change_lag() {
    let mut instrument = new_instrument();
    instrument.prepare().await.expect("prepare() must succeed");
    assert_eq!(instrument.id().bus, BUS);
    assert_eq!(instrument.id().port_path, PORT_PATH.to_vec());
    let (fw, revid) = instrument.firmware_info().expect("ready after prepare()");
    eprintln!("firmware {fw:?}, revid {revid}");

    for (rate_hz, samples) in [
        (20_000u64, 50_000u64),
        (25_000, 50_000),
        (1_000_000, 2_000_000),
        // The device's ceiling, which came down from 24 MHz to 16 MHz in B-63:
        // 24 MHz streams on a quiet bus and stalls about half the time on a
        // shared one, and `configure` refuses it now.
        (16_000_000, 16_000_000),
    ] {
        let (received, stats, _) = capture(&mut instrument, rate_hz, samples).await;
        assert!(stats.primed, "every rate change must prime ({rate_hz} Hz)");
        assert_clean(rate_hz, samples, &received, &stats);
    }

    // Same rate again: nothing to absorb, so no priming.
    let (received, stats, _) = capture(&mut instrument, 16_000_000, 16_000_000).await;
    assert!(!stats.primed, "a repeated rate must not prime");
    assert_clean(16_000_000, 16_000_000, &received, &stats);
}

/// The lag lives in the device, not in the driver: a new instance — as after
/// a server restart — must still sample correctly after a *previous* one
/// left the hardware on a slow rate. This is the case a per-process "last
/// rate" alone would miss.
#[tokio::test]
#[ignore = "needs root and the real fx2lafw logic analyzer at bus path 3-3.4.1.1"]
async fn a_new_instance_after_a_slow_capture_samples_at_the_requested_rate() {
    {
        let mut previous = new_instrument();
        previous.prepare().await.expect("prepare() must succeed");
        let (received, stats, _) = capture(&mut previous, 20_000, 50_000).await;
        assert_clean(20_000, 50_000, &received, &stats);
    }
    let mut fresh = new_instrument();
    fresh.prepare().await.expect("prepare() must succeed");
    let (received, stats, _) = capture(&mut fresh, 1_000_000, 2_000_000).await;
    assert!(stats.primed, "the first capture after prepare() must prime");
    assert_clean(1_000_000, 2_000_000, &received, &stats);
}

/// 48 MHz is refused up front rather than attempted: 8 channels at 48 MHz is
/// 48 MB/s, beyond what USB 2.0 bulk sustains, so the FIFO overruns and the
/// device sends nothing.
///
/// Before this refusal existed, this test ran the capture and verified on the
/// real device that the driver's stall detection ends it (`stalled == true`,
/// 0 bytes, ended after 1.01 s) — the exact case that once hung this suite
/// for over four hours. Stall detection stays in the driver as the defence
/// for any other silence; this test now pins the refusal.
#[tokio::test]
#[ignore = "needs root and the real fx2lafw logic analyzer at bus path 3-3.4.1.1"]
async fn rate_beyond_usb2_bandwidth_is_refused_at_configure() {
    let mut instrument = new_instrument();
    instrument.prepare().await.expect("prepare() must succeed");
    let err = instrument
        .configure(AcqConfig {
            rate_hz: 48_000_000,
            channels: (0..8).collect(),
            samples: 1_000,
            trigger: None,
            ranges_v: vec![],
            continuous: false,
        })
        .await
        .expect_err("48 MHz must be refused");
    eprintln!("refused as expected: {err}");
    assert!(
        err.to_string().contains("USB 2.0"),
        "the refusal must say why: {err}"
    );
}

/// `prepare()` must be safe (and correct) to call again on an
/// already-programmed device — SPECIFICATION.md §2.5: "Runs on EVERY
/// enumeration. FX2 devices hold firmware in RAM and lose it on power
/// loss" — so a second call, with firmware already resident, must still
/// walk the whole upload + re-enumerate + re-query sequence successfully
/// rather than assuming a one-shot init.
#[tokio::test]
#[ignore = "needs root and the real fx2lafw logic analyzer at bus path 3-3.4.1.1"]
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

    // Bus path identity must be unchanged (DeviceId equality ignores
    // vid/pid, SPECIFICATION.md §2.3) even though a fresh disconnect/
    // re-enumerate cycle happened in between.
    assert_eq!(after_first, after_second);
    eprintln!("prepare() #1 -> {after_first}, prepare() #2 -> {after_second}: same bus path");
}

/// A short capture must be cleanly cancellable mid-flight via `stop()`
/// without hanging or panicking, and the driver's own stats must reflect
/// fewer samples than requested (since we deliberately stop early).
#[tokio::test]
#[ignore = "needs root and the real fx2lafw logic analyzer at bus path 3-3.4.1.1"]
async fn stop_cancels_a_long_capture_promptly() {
    let mut instrument = new_instrument();
    instrument.prepare().await.expect("prepare() must succeed");

    instrument
        .configure(AcqConfig {
            rate_hz: 1_000_000,
            channels: (0..8).collect(),
            // A long capture (10 s at 1 MS/s) that stop() should cut off
            // long before it would finish on its own.
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
        "stop() must cancel promptly, not wait for the full capture"
    );

    drop(rx); // the sender side is already gone once the task exits.
    let stats = instrument.last_stats();
    eprintln!(
        "stopped early: received {} of {} requested samples, dropped_transfers={}",
        stats.received_samples, stats.requested_samples, stats.dropped_transfers
    );
    assert!(stats.received_samples < stats.requested_samples);
}

/// Rising edges of D0 (bit 0) in a packed-digital byte stream — every other
/// bit is masked off host-side by `decode_samples` when only channel 0 is
/// requested, so a nonzero byte here means D0 was high.
fn d0_rising_edges(bytes: &[u8]) -> Vec<usize> {
    let mut edges = Vec::new();
    let mut high = false;
    for (i, &b) in bytes.iter().enumerate() {
        let level = b & 1 != 0;
        if level && !high {
            edges.push(i);
        }
        high = level;
    }
    edges
}

/// Runs a continuous acquisition (SPECIFICATION.md §2.3.2) at `rate_hz` for
/// `run_for`, then stops it. Returns the concatenated D0 bytes, the
/// wall-clock span between the first and last chunk to arrive at this test,
/// the sample count produced *after* the first chunk (excluding it mirrors
/// `fx2lafw::driver`'s own `measured_rate`: the first chunk's arrival marks
/// the end of its own fill time, not the start of a steady rate), the
/// driver's stats, and how long `stop()` took to return.
async fn run_continuous(
    instrument: &mut Fx2lafwInstrument,
    rate_hz: u64,
    run_for: Duration,
) -> (Vec<u8>, Duration, u64, AcquisitionStats, Duration) {
    instrument
        .configure(AcqConfig {
            rate_hz,
            channels: vec![0],
            samples: 0, // ignored in continuous mode.
            trigger: None,
            ranges_v: vec![],
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

/// The continuous-acquisition acceptance check: 60 s continuous at 1 MHz on the
/// fx2lafw logic analyzer, no stall, the sample count within 0.5 % of rate x elapsed,
/// `stop()` within 1 s, and D0 — wired to the oscilloscope's CAL output — still
/// reading 1 kHz at the end. Also checks that a normal finite capture works
/// right after — no state a continuous run could have left behind.
#[tokio::test]
#[ignore = "needs root, the real fx2lafw logic analyzer at bus path 3-3.4.1.1, D0 wired to CAL, runs for 60 s"]
async fn continuous_acquisition_at_1mhz_runs_for_60s_without_stalling() {
    let mut instrument = new_instrument();
    instrument.prepare().await.expect("prepare() must succeed");

    let rate_hz = 1_000_000u64;
    let (bytes, elapsed, samples_after_first, stats, stop_elapsed) =
        run_continuous(&mut instrument, rate_hz, Duration::from_secs(60)).await;

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

    let tail_len = (rate_hz as usize).min(bytes.len());
    let tail = &bytes[bytes.len() - tail_len..];
    let edges = d0_rising_edges(tail);
    let periods: Vec<f64> = edges.windows(2).map(|w| (w[1] - w[0]) as f64).collect();
    assert!(
        periods.len() >= 2,
        "not enough edges on D0 in the final second to measure CAL ({} edges)",
        edges.len()
    );
    let mean = periods.iter().sum::<f64>() / periods.len() as f64;
    let freq = rate_hz as f64 / mean;
    eprintln!(
        "final second: CAL on D0 {freq:.4} Hz over {} periods",
        periods.len()
    );
    assert!(
        (freq - 1000.0).abs() <= 1.0,
        "CAL on D0 read {freq:.4} Hz in the final second: the timebase drifted"
    );

    // A normal finite capture right after a continuous one must work
    // cleanly -- no leftover state (e.g. a stale cancel channel or stats).
    let (received, stats, _) = capture(&mut instrument, rate_hz, rate_hz / 2).await;
    assert_clean(rate_hz, rate_hz / 2, &received, &stats);
}

/// A short capture at the top rate — what a UI's Single mode asks for by
/// default (1024 samples at 24 MHz), after the idle time a server has between
/// start-up and its first capture. Once seen to deliver nothing and stall in
/// the container (2026-09-15), though multi-million-sample captures at the
/// same rate were clean.
///
/// It recurred on 2026-09-16, and that time it stayed: for the rest of that
/// server's life *every* 24 MHz start received 0 bytes — captures and
/// recordings alike — while 12 and 16 MHz worked immediately before and
/// after, and powering the other devices off changed nothing. Restarting the
/// server fixed it, which is what identified the cure: `prepare()` re-uploads
/// the firmware, and that resets the 8051. The driver now does exactly that
/// by itself when two starts in a row receive nothing (see `start()`), so a
/// wedged device costs a re-enumeration instead of the session.
/// The session that wedged the device (2026-09-16) was a person clicking:
/// Single pressed again before the last one finished, recordings started and
/// stopped seconds apart, over and over at the top rate. Each of those is
/// ordinary; the driver must come out of them still able to stream.
#[tokio::test]
#[ignore = "needs root and the real fx2lafw logic analyzer at bus path 3-3.4.1.1"]
async fn the_top_rate_survives_superseded_captures_and_torn_down_recordings() {
    let mut instrument = new_instrument();
    instrument.prepare().await.expect("prepare() must succeed");

    for round in 0..4 {
        // Supersede: start a long capture, then start another without
        // waiting. `start()` must stop the first one itself.
        for _ in 0..2 {
            instrument
                .configure(AcqConfig {
                    rate_hz: 16_000_000,
                    channels: (0..8).collect(),
                    samples: 32_000_000, // 2 s, far longer than we wait
                    trigger: None,
                    ranges_v: vec![],
                    continuous: false,
                })
                .await
                .expect("configure must succeed");
            let (sink, _rx) = SampleSink::channel(64);
            instrument.start(sink).await.expect("start must succeed");
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        instrument.stop().await.expect("stop must succeed");

        // And a recording torn down mid-stream.
        instrument
            .configure(AcqConfig {
                rate_hz: 16_000_000,
                channels: (0..8).collect(),
                samples: 0,
                trigger: None,
                ranges_v: vec![],
                continuous: true,
            })
            .await
            .expect("configure must succeed");
        let (sink, _rx) = SampleSink::channel(64);
        instrument.start(sink).await.expect("start must succeed");
        tokio::time::sleep(Duration::from_millis(600)).await;
        instrument.stop().await.expect("stop must succeed");

        // The point of the test: an ordinary capture still streams.
        let (received, stats, _) = capture(&mut instrument, 16_000_000, 1024).await;
        assert!(!stats.stalled, "stalled after round {round}");
        assert_eq!(received.len(), 1024, "round {round} delivered nothing");
    }
}

/// B-26 (live test round 4, 2026-09-30): back-to-back captures at the top of
/// the rate ladder are not individually reliable. Ten 12 M-sample captures at
/// 24 MHz failed once at exactly zero samples, and twenty failed once, with
/// every neighbour delivering all 12 M with no dropped transfers; priming
/// each start first (the B-20 remedy) changed nothing, because the priming
/// acquisition itself streams fine. The driver answers a start that delivers
/// *nothing* by starting it again, so what the caller sees is a capture that
/// took a stall timeout longer, not one that failed.
///
/// Captures come from `AWIC_B26_CAPTURES` (default 20); with a 1-in-10 rate
/// and no retry, twenty captures fail about 88 % of the time.
#[tokio::test]
#[ignore = "needs root and the real fx2lafw logic analyzer at bus path 3-3.4.1.1; ~30 s"]
async fn a_start_that_delivers_nothing_at_the_top_rate_is_restarted_not_failed() {
    let mut instrument = new_instrument();
    instrument.prepare().await.expect("prepare() must succeed");

    let rounds: usize = std::env::var("AWIC_B26_CAPTURES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let mut restarted = 0usize;
    for round in 0..rounds {
        let (received, stats, _) = capture(&mut instrument, 16_000_000, 12_000_000).await;
        println!(
            "  {:2}: received={} stalled={} silent_restarts={}",
            round + 1,
            received.len(),
            stats.stalled,
            stats.silent_restarts
        );
        if stats.silent_restarts > 0 {
            restarted += 1;
        }
        assert!(!stats.stalled, "capture {} stalled", round + 1);
        assert_eq!(
            received.len(),
            12_000_000,
            "capture {} came back short",
            round + 1
        );
    }
    println!("{restarted} of {rounds} captures needed a restart");
}

/// B-25 (live test round 3, 2026-09-19): after about 16 minutes of use —
/// the analyzer streaming at 12 MHz, part of it alongside the scope —
/// *every* 24 MHz acquisition started and then stalled 8-450 ms in, while
/// 16 MHz stayed clean; restarting the server (which re-uploads the
/// firmware) restored 24 MHz completely. This reproduces that load with both
/// instruments driven directly, then asks for top-rate captures. A stall
/// may happen — the degradation is the premise — but the driver must come
/// back from it: after any stalled capture the next one has to be clean,
/// because `start()` now resets the device after a stall with data.
///
/// Minutes of load come from `AWIC_B25_MINUTES` (default 16).
#[tokio::test]
#[ignore = "needs the real fx2lafw logic analyzer at 3-3.4.1.1 and oscilloscope at 3-3.4.2; ~20 minutes"]
async fn a_stall_at_the_top_rate_after_long_use_is_recovered_from() {
    use awic_native::fx2scope::Fx2ScopeInstrument;

    let minutes: u64 = std::env::var("AWIC_B25_MINUTES")
        .ok()
        .and_then(|m| m.parse().ok())
        .unwrap_or(16);

    let mut analyzer = new_instrument();
    analyzer.prepare().await.expect("analyzer prepare()");
    let mut scope = Fx2ScopeInstrument::new(
        DeviceId::new(3, vec![3, 4, 2], 0, 0),
        Arc::new(FirmwareResolver::default()),
    );
    scope.prepare().await.expect("scope prepare()");

    // The load: both streaming continuously, drained as fast as they come.
    let continuous = |rate_hz, channels: Vec<u8>, ranges_v: Vec<f32>| AcqConfig {
        rate_hz,
        channels,
        samples: 0,
        trigger: None,
        ranges_v,
        continuous: true,
    };
    let mut stalls_under_load = 0usize;
    let mut previous_stalled_with_data = false;
    let mut resets_seen = 0usize;
    let deadline = Instant::now() + Duration::from_secs(60 * minutes);
    scope
        .configure(continuous(4_000_000, vec![0], vec![1.25]))
        .await
        .expect("scope configure");
    let (scope_sink, scope_rx) = SampleSink::channel(64);
    scope.start(scope_sink).await.expect("scope start");
    let scope_drain = tokio::spawn(drain(scope_rx));
    while Instant::now() < deadline {
        // The analyzer in segments, so a stall under load is counted and
        // the load goes on rather than ending the test.
        analyzer
            .configure(continuous(12_000_000, (0..8).collect(), vec![]))
            .await
            .expect("analyzer configure");
        let (sink, rx) = SampleSink::channel(64);
        analyzer.start(sink).await.expect("analyzer start");
        let segment = tokio::spawn(drain(rx));
        let left = deadline.saturating_duration_since(Instant::now());
        tokio::time::sleep(left.min(Duration::from_secs(120))).await;
        analyzer.stop().await.expect("analyzer stop");
        let _ = segment.await;
        let stats = analyzer.last_stats();
        eprintln!(
            "12 MHz segment: {} samples, stalled={}, reset_before={}",
            stats.received_samples, stats.stalled, stats.reset_before
        );
        // The mechanism itself, checked on every segment: one that follows a
        // stall with data must have begun with a device reset (B-25).
        if previous_stalled_with_data {
            assert!(
                stats.reset_before,
                "the segment after a mid-stream stall began without a device reset"
            );
            resets_seen += 1;
        }
        previous_stalled_with_data = stats.stalled && stats.received_samples > 0;
        if stats.stalled {
            stalls_under_load += 1;
        }
    }
    scope.stop().await.expect("scope stop");
    let _ = scope_drain.await;
    eprintln!(
        "{minutes} min of load done; 12 MHz stalls under load: {stalls_under_load}, \
         device resets after them: {resets_seen}"
    );

    // The point: top-rate captures after the load.
    let mut outcomes = Vec::new();
    for _ in 0..8 {
        let (received, stats, _) = capture(&mut analyzer, 16_000_000, 12_000_000).await;
        outcomes.push((stats.stalled, received.len()));
    }
    eprintln!("24 MHz after the load (stalled, samples): {outcomes:?}");
    for pair in outcomes.windows(2) {
        assert!(
            !(pair[0].0 && pair[1].0),
            "two stalled top-rate captures in a row — the reset after a stall did not \
             recover the device: {outcomes:?}"
        );
    }
    assert!(
        !outcomes.last().unwrap().0,
        "the last top-rate capture stalled: {outcomes:?}"
    );
}

#[tokio::test]
#[ignore = "needs root and the real fx2lafw logic analyzer at bus path 3-3.4.1.1"]
async fn a_short_capture_at_the_top_rate_delivers_every_sample() {
    let mut instrument = new_instrument();
    instrument.prepare().await.expect("prepare() must succeed");
    // A server prepares at start-up and the first capture comes much later.
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    for samples in [1024u64, 4096, 65_536, 1024] {
        let (received, stats, _) = capture(&mut instrument, 16_000_000, samples).await;
        eprintln!(
            "{samples} samples: received {} stalled={}",
            received.len(),
            stats.stalled
        );
        // Too short to rate-check: count and stall only.
        assert!(!stats.stalled, "stalled on a {samples}-sample capture");
        assert_eq!(received.len() as u64, samples);
    }
}
