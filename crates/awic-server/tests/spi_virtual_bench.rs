//! A virtual SPI bench: a simulated master and slave in each of the four SPI
//! modes, sampled the way the logic analyzer samples a real bus, then run
//! through the same path an agent uses — `survey` to find the bus and its
//! mode, `decode` with what the survey said — and every byte checked.
//!
//! The waveform is built in continuous time (nanoseconds) and only then
//! sampled, so the things a real bus does to a decoder are in it: data that
//! changes a little after the shift edge — for CPHA=0 that is the previous
//! bit's trailing edge, exactly the edge a wrong mode would sample at (so at the analyzer's rate it lands
//! on the clock edge's own sample, or the one after), an asymmetric clock,
//! edge jitter, gaps between bytes and transfers, CS setup and hold, and a
//! MISO line that floats high (pulled up) while the slave is not selected.

use awic_core::capture_store::derive_transitions;
use awic_core::{Capture, CaptureData, CaptureId, CaptureMeta, DeviceId};
use awic_proto::frame::Encoding;
use awic_proto::{
    ChannelMapping, ChannelRole, DecodeFormat, DecoderId, OptionSetting, OptionValue, ProtocolGuess,
};
use awic_server::decode::{decode_capture, decode_capture_range, DecodeSpec};
use awic_server::survey::survey_capture;

/// Analyzer channels the simulated bus is wired to.
#[derive(Clone, Copy)]
struct Wiring {
    clk: u8,
    mosi: u8,
    miso: u8,
    cs: u8,
}

const WIRING: Wiring = Wiring {
    clk: 0,
    mosi: 1,
    miso: 2,
    cs: 3,
};

/// One simulated bus setup.
#[derive(Clone, Copy)]
struct Bench {
    mode: u8,
    sclk_hz: f64,
    rate_hz: u64,
    /// How long after the shift edge MOSI and MISO change.
    skew_ns: f64,
    /// Share of the clock period spent at the active (non-idle) level.
    active_share: f64,
    /// Peak edge jitter, as a share of the half period.
    jitter: f64,
    cs_active_high: bool,
    wiring: Wiring,
}

impl Bench {
    fn new(mode: u8, sclk_hz: f64) -> Self {
        Bench {
            mode,
            sclk_hz,
            rate_hz: 16_000_000,
            skew_ns: 15.0,
            active_share: 0.45,
            jitter: 0.05,
            cs_active_high: false,
            wiring: WIRING,
        }
    }
}

/// The transfers every bench sends: `(mosi, miso)` byte pairs. Patterns a
/// decoder gets wrong in different ways — all zeros and all ones (no data
/// edges at all), alternating bits (an edge every bit), a walking one, and
/// a run of pseudo-random bytes.
fn transfers() -> Vec<Vec<(u8, u8)>> {
    let mut seed = 0x2545_F491u32;
    let mut random = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed as u8
    };
    let mut out = vec![
        vec![(0x9F, 0xFF), (0x00, 0xEF), (0x00, 0x40), (0x00, 0x18)], // JEDEC ID read
        vec![(0x00, 0x00), (0xFF, 0xFF)],
        vec![(0xAA, 0x55), (0x55, 0xAA)],
        (0..8).map(|i| (1u8 << i, 0x80u8 >> i)).collect(),
    ];
    for _ in 0..4 {
        out.push((0..16).map(|_| (random(), random())).collect());
    }
    out
}

/// A level change on one line, in nanoseconds.
struct Event {
    at_ns: f64,
    bit: u8,
    high: bool,
}

/// Simulates the bus and samples it: the packed capture the analyzer would
/// deliver.
fn simulate(b: &Bench, payload: &[Vec<(u8, u8)>]) -> Vec<u8> {
    let cpol = b.mode >= 2;
    let cpha = b.mode % 2 == 1;
    let period = 1e9 / b.sclk_hz;
    let active = period * b.active_share;
    let idle = period - active;
    let w = b.wiring;
    let cs_on = b.cs_active_high;
    let mut jitter_seed = 0x9E37_79B9u32;
    let mut jitter = |half: f64| {
        jitter_seed = jitter_seed
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        let unit = (jitter_seed >> 8) as f64 / (1u32 << 24) as f64 * 2.0 - 1.0;
        unit * b.jitter * half
    };

    let mut ev = Vec::new();
    let set =
        |ev: &mut Vec<Event>, at_ns: f64, bit: u8, high: bool| ev.push(Event { at_ns, bit, high });
    // Idle: clock at CPOL, CS released, MOSI low, MISO floating high.
    set(&mut ev, 0.0, w.clk, cpol);
    set(&mut ev, 0.0, w.cs, !cs_on);
    set(&mut ev, 0.0, w.mosi, false);
    set(&mut ev, 0.0, w.miso, true);

    let mut t = 20_000.0; // 20 µs of idle bus first
    for transfer in payload {
        set(&mut ev, t, w.cs, cs_on);
        t += period; // CS setup
                     // CPHA=0: where the next bit is shifted out — the first one as soon
                     // as CS is asserted, each later one at the previous trailing edge.
        let mut shift_at = t - period / 2.0;
        for (n, &(mosi, miso)) in transfer.iter().enumerate() {
            if n > 0 {
                t += 1.5 * period; // the master's gap between bytes
            }
            for i in (0..8).rev() {
                let (o, s) = ((mosi >> i) & 1 == 1, (miso >> i) & 1 == 1);
                let lead = t + jitter(active.min(idle));
                let trail = t + active + jitter(active.min(idle));
                if !cpha {
                    // Data must be valid at the leading (sampling) edge: it
                    // was shifted out at the previous trailing edge.
                    set(&mut ev, shift_at + b.skew_ns, w.mosi, o);
                    set(&mut ev, shift_at + b.skew_ns, w.miso, s);
                    shift_at = trail;
                } else {
                    // Shifted on the leading edge, sampled on the trailing.
                    set(&mut ev, lead + b.skew_ns, w.mosi, o);
                    set(&mut ev, lead + b.skew_ns, w.miso, s);
                }
                set(&mut ev, lead, w.clk, !cpol);
                set(&mut ev, trail, w.clk, cpol);
                t += period;
            }
        }
        t += period; // CS hold
        set(&mut ev, t, w.cs, !cs_on);
        set(&mut ev, t + 2.0 * period, w.miso, true); // slave releases MISO
        set(&mut ev, t + 2.0 * period, w.mosi, false);
        t += 200_000.0; // 200 µs between transfers
    }
    t += 20_000.0;

    ev.sort_by(|a, b| a.at_ns.total_cmp(&b.at_ns));
    let samples = (t * b.rate_hz as f64 / 1e9) as usize;
    let mut packed = Vec::with_capacity(samples);
    let mut level = 0u8;
    let mut next = 0;
    for n in 0..samples {
        let now = n as f64 * 1e9 / b.rate_hz as f64;
        while next < ev.len() && ev[next].at_ns <= now {
            let e = &ev[next];
            level = if e.high {
                level | 1 << e.bit
            } else {
                level & !(1 << e.bit)
            };
            next += 1;
        }
        packed.push(level);
    }
    packed
}

fn capture(packed: Vec<u8>, rate_hz: u64) -> Capture {
    Capture {
        id: CaptureId::new(),
        meta: CaptureMeta {
            device: DeviceId::new(3, vec![3, 4, 1], 0x0925, 0x3881),
            rate_hz,
            channel_count: 8,
            channels: (0..8).collect(),
            encoding: Encoding::PackedDigital,
            t0_ns: 0,
            trigger_position: None,
        },
        data: CaptureData::Digital {
            transitions: derive_transitions(&packed),
            packed,
        },
    }
}

/// What the survey said about the bus.
#[derive(Debug, PartialEq)]
struct Found {
    clk: u8,
    data: Vec<u8>,
    cs: Option<u8>,
    cs_active_high: bool,
    mode: u8,
    clock_hz: Option<f64>,
    undersampled: bool,
}

fn survey_spi(c: &Capture) -> Option<Found> {
    let result = survey_capture(c, "capture://virtual".into(), None, None).unwrap();
    let spi: Vec<_> = result
        .guesses
        .iter()
        .filter_map(|g| match g {
            ProtocolGuess::Spi {
                clk,
                data,
                cs,
                cs_active_high,
                mode,
                clock_hz,
                undersampled,
            } => Some(Found {
                clk: *clk,
                data: data.clone(),
                cs: *cs,
                cs_active_high: *cs_active_high,
                mode: *mode,
                clock_hz: *clock_hz,
                undersampled: *undersampled,
            }),
            _ => None,
        })
        .collect();
    assert!(spi.len() <= 1, "more than one SPI bus: {spi:?}");
    spi.into_iter().next()
}

/// `(mosi, miso)` hex per transaction.
fn decode_spi(
    c: &Capture,
    w: Wiring,
    mode: u8,
    cs_active_high: bool,
) -> Vec<(String, Option<String>)> {
    let channels = [
        (ChannelRole::Clk, w.clk),
        (ChannelRole::Mosi, w.mosi),
        (ChannelRole::Miso, w.miso),
        (ChannelRole::Cs, w.cs),
    ]
    .map(|(role, channel)| ChannelMapping {
        role,
        channel,
        minus: None,
    });
    let options = [
        OptionSetting {
            id: "mode".into(),
            value: OptionValue::Text(format!("mode{mode}")),
        },
        OptionSetting {
            id: "cs_active_high".into(),
            value: OptionValue::Bool(cs_active_high),
        },
    ];
    let out = decode_capture(
        c,
        &DecodeSpec {
            decoder: DecoderId::Spi,
            channels: &channels,
            options: &options,
            max_annotations: Some(5000),
            max_level: None,
            format: Some(DecodeFormat::Transactions),
            max_bytes: None,
            fold: None,
        },
    )
    .unwrap();
    assert!(!out.truncated);
    out.transactions
        .into_iter()
        .map(|t| (t.data, t.miso))
        .collect()
}

fn expected(payload: &[Vec<(u8, u8)>]) -> Vec<(String, Option<String>)> {
    let hex = |b: Vec<u8>| {
        b.iter()
            .map(|x| format!("{x:02X}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    payload
        .iter()
        .map(|t| {
            (
                hex(t.iter().map(|p| p.0).collect()),
                Some(hex(t.iter().map(|p| p.1).collect())),
            )
        })
        .collect()
}

/// Runs one bench the way an agent would: survey, then decode with the
/// wiring and mode the survey reported. Returns the survey's finding and
/// whether every byte of every transfer came back.
fn run(b: &Bench) -> (Option<Found>, bool) {
    let payload = transfers();
    let c = capture(simulate(b, &payload), b.rate_hz);
    let Some(found) = survey_spi(&c) else {
        return (None, false);
    };
    // The survey does not know MOSI from MISO; the bench does.
    let ok = found.data.len() == 2
        && found.cs.is_some()
        && decode_spi(&c, b.wiring, found.mode, found.cs_active_high) == expected(&payload);
    (Some(found), ok)
}

#[test]
fn every_mode_is_found_and_decoded_at_typical_clock_rates() {
    // 16 MS/s, the analyzer's sustained top rate; clocks from a slow sensor
    // bus up to 2 MHz (8 samples per clock period).
    for sclk in [100e3, 250e3, 500e3, 1e6, 2e6] {
        for mode in 0..4 {
            let b = Bench::new(mode, sclk);
            let (found, ok) = run(&b);
            let found =
                found.unwrap_or_else(|| panic!("mode {mode} at {sclk} Hz: no SPI bus found"));
            assert_eq!(
                (
                    found.clk,
                    &found.data[..],
                    found.cs,
                    found.cs_active_high,
                    found.mode
                ),
                (0, &[1, 2][..], Some(3), false, mode),
                "mode {mode} at {sclk} Hz"
            );
            let hz = found.clock_hz.expect("a clock rate");
            assert!(
                (hz - sclk).abs() <= 0.15 * sclk,
                "mode {mode}: clock {hz} Hz, sent {sclk} Hz"
            );
            assert!(ok, "mode {mode} at {sclk} Hz: bytes differ");
        }
    }
}

#[test]
fn data_changing_on_the_clock_edges_own_sample_still_gives_the_mode() {
    // No skew at all: MOSI/MISO change at the very sample the shift edge
    // lands on — the case where only "which edge they coincide with" tells
    // the modes apart.
    for mode in 0..4 {
        let b = Bench {
            skew_ns: 0.0,
            jitter: 0.0,
            ..Bench::new(mode, 1e6)
        };
        let (found, ok) = run(&b);
        assert_eq!(found.map(|f| f.mode), Some(mode));
        assert!(ok, "mode {mode}");
    }
}

#[test]
fn a_late_slave_and_an_active_high_chip_select() {
    // MISO 200 ns after the shift edge at 1 MHz (a fifth of the period) and
    // CS active high: the survey must say which level is asserted for the
    // decode to frame the transfers at all.
    for mode in 0..4 {
        let b = Bench {
            skew_ns: 200.0,
            cs_active_high: true,
            ..Bench::new(mode, 1e6)
        };
        let (found, ok) = run(&b);
        assert_eq!(
            found.map(|f| (f.mode, f.cs, f.cs_active_high)),
            Some((mode, Some(3), true))
        );
        assert!(ok, "mode {mode}");
    }
}

#[test]
fn any_wiring_is_found() {
    let wiring = Wiring {
        clk: 6,
        mosi: 4,
        miso: 0,
        cs: 7,
    };
    for mode in 0..4 {
        let b = Bench {
            wiring,
            ..Bench::new(mode, 500e3)
        };
        let payload = transfers();
        let c = capture(simulate(&b, &payload), b.rate_hz);
        let found = survey_spi(&c).expect("an SPI bus");
        assert_eq!(
            (found.clk, &found.data[..], found.cs, found.mode),
            (6, &[0, 4][..], Some(7), mode)
        );
        assert_eq!(
            decode_spi(&c, wiring, found.mode, false),
            expected(&payload)
        );
    }
}

/// The decoder reads data at the sampling edge only. Modes that share it
/// (0 and 3: rising; 1 and 2: falling) decode the same bytes — they differ
/// in where the clock rests, which the survey reports — while a mode with
/// the other sampling edge reads the bits half a clock off.
#[test]
fn the_wrong_sampling_edge_gives_wrong_bytes_the_right_one_does_not() {
    let payload = transfers();
    let rising = |m: u8| m == 0 || m == 3;
    for sent in 0..4u8 {
        let b = Bench::new(sent, 1e6);
        let c = capture(simulate(&b, &payload), b.rate_hz);
        for decoded_as in 0..4u8 {
            let got = decode_spi(&c, WIRING, decoded_as, false);
            if rising(sent) == rising(decoded_as) {
                assert_eq!(
                    got,
                    expected(&payload),
                    "sent {sent}, decoded as {decoded_as}"
                );
            } else {
                assert_ne!(
                    got,
                    expected(&payload),
                    "sent {sent}, decoded as {decoded_as}"
                );
            }
        }
    }
}

/// Where the method stops working, printed for the record and pinned so a
/// change that moves the limit is noticed. At 16 MS/s, 4 MHz is 4 samples per
/// clock period — a half period of 2 samples, a glitch by the survey's count.
/// Such a clock used to be called crosstalk (B-100, a 5.5 MHz MCP2515 bus on
/// the live bench); now it is found as `undersampled`, with the right mode,
/// up to 5.5 MHz. At 7 MHz (2.3 samples per period) the survey may find
/// nothing, but never a wrong mode, and the decoder still reads every byte.
#[test]
fn the_limit_is_four_samples_per_clock_period() {
    let mut table = String::from("\nsclk      mode  survey                       decode\n");
    for sclk in [2e6, 3.2e6, 4e6, 5.5e6, 7e6] {
        for mode in 0..4 {
            let b = Bench {
                jitter: 0.0,
                ..Bench::new(mode, sclk)
            };
            let payload = transfers();
            let c = capture(simulate(&b, &payload), b.rate_hz);
            let found = survey_spi(&c);
            let decoded = decode_spi(&c, WIRING, mode, false) == expected(&payload);
            table.push_str(&format!(
                "{:>4.1} MHz  {mode}     {:<28} {}\n",
                sclk / 1e6,
                found.as_ref().map_or("none".to_string(), |f| format!(
                    "mode {} clk D{} data {:?}{}",
                    f.mode,
                    f.clk,
                    f.data,
                    if f.undersampled { " (fast)" } else { "" }
                )),
                if decoded { "all bytes" } else { "WRONG" },
            ));
            if sclk <= 5.5e6 {
                assert_eq!(found.as_ref().map(|f| f.mode), Some(mode), "{sclk} Hz");
            }
            if let Some(f) = &found {
                assert_eq!(f.mode, mode, "{sclk} Hz: a wrong mode");
                assert_eq!(f.undersampled, sclk >= 4e6, "{sclk} Hz");
                assert_eq!(f.clock_hz.is_none(), f.undersampled, "{sclk} Hz");
            }
            assert!(decoded, "mode {mode} at {sclk} Hz: the decoder lost bytes");
        }
    }
    eprintln!("{table}");
}

/// B-101: `decode` over `[t0_s, t1_s)` of a capture used to ignore the window
/// and decode it all. Two windows split between transfers give exactly the
/// whole decode, with sample numbers still counted from the capture's start;
/// a window opened inside a transfer waits for the next chip select instead
/// of inventing bytes from the middle of one.
#[test]
fn a_capture_decodes_over_a_window() {
    let b = Bench {
        jitter: 0.0,
        ..Bench::new(0, 1e6)
    };
    let c = capture(simulate(&b, &transfers()), b.rate_hz);
    let rate = b.rate_hz as f64;
    let channels = [
        (ChannelRole::Clk, WIRING.clk),
        (ChannelRole::Mosi, WIRING.mosi),
        (ChannelRole::Miso, WIRING.miso),
        (ChannelRole::Cs, WIRING.cs),
    ]
    .map(|(role, channel)| ChannelMapping {
        role,
        channel,
        minus: None,
    });
    let spec = DecodeSpec {
        decoder: DecoderId::Spi,
        channels: &channels,
        options: &[],
        max_annotations: Some(5000),
        max_level: None,
        format: Some(DecodeFormat::Transactions),
        max_bytes: None,
        fold: None,
    };
    let window = |t0: Option<f64>, t1: Option<f64>| {
        decode_capture_range(&c, &spec, t0, t1)
            .unwrap()
            .transactions
            .into_iter()
            .map(|t| (t.start_sample, t.data))
            .collect::<Vec<_>>()
    };
    let all = decode_capture(&c, &spec)
        .unwrap()
        .transactions
        .into_iter()
        .map(|t| (t.start_sample, t.data))
        .collect::<Vec<_>>();
    assert_eq!(window(None, None), all);
    assert!(all.len() >= 4, "{all:?}");

    // Split in the gap between the third and the fourth transfer.
    let ends: Vec<u64> = decode_capture(&c, &spec)
        .unwrap()
        .transactions
        .iter()
        .map(|t| t.end_sample)
        .collect();
    let split = (ends[2] + all[3].0) as f64 / 2.0 / rate;
    let mut joined = window(None, Some(split));
    assert_eq!(joined.len(), 3);
    joined.extend(window(Some(split), None));
    assert_eq!(joined, all);

    // Opened in the middle of the fourth transfer: it starts at the fifth.
    let mid = (all[3].0 + ends[3]) as f64 / 2.0 / rate;
    assert_eq!(window(Some(mid), None), all[4..].to_vec());

    // An empty or out-of-range window is refused, not silently widened.
    let err = decode_capture_range(&c, &spec, Some(split), Some(split)).unwrap_err();
    assert_eq!(err.code, "empty_range");
    let past = (c_len(&c) + 10) as f64 / rate;
    let err = decode_capture_range(&c, &spec, Some(past), None).unwrap_err();
    assert_eq!(err.code, "empty_range");
}

fn c_len(c: &Capture) -> u64 {
    match &c.data {
        CaptureData::Digital { packed, .. } => packed.len() as u64,
        _ => unreachable!(),
    }
}
