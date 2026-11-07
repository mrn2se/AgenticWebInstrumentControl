//! The virtual instruments capture traffic AWIC's own decoders can read:
//! the boot banner on UART, the sensor poll on I²C, the flash probe on SPI,
//! the counter puzzle's segment pins, and the 555 on the scope.
//!
//! Runs on tokio's paused clock, so a capture of simulated time costs no
//! wall time waiting.

use awic_core::{AcqConfig, Backend, SampleSink};
use awic_decode::i2c::{I2cConfig, I2cDecoder};
use awic_decode::spi::{BitOrder as SpiBitOrder, SpiConfig, SpiDecoder, SpiMode};
use awic_decode::uart::{BitOrder, Parity, StopBits, UartConfig, UartDecoder};
use awic_decode::{AnnotationKind, Decoder, Transition};
use awic_sim::{SimBackend, SimBench, SimSettings};

const RATE: u64 = 8_000_000;

fn bench() -> SimBench {
    SimBench::new(&SimSettings {
        seed: 11,
        faults: vec![],
        shuffle: false,
    })
    .unwrap()
}

async fn capture(bench: &SimBench, device: awic_core::DeviceId, cfg: AcqConfig) -> Vec<u8> {
    let backend = SimBackend::new(bench.clone());
    let mut inst = backend.open(&device).await.unwrap();
    inst.prepare().await.unwrap();
    inst.configure(cfg).await.unwrap();
    let (sink, mut rx) = SampleSink::channel(1024);
    inst.start(sink).await.unwrap();
    let mut data = Vec::new();
    while let Some(chunk) = rx.recv().await {
        data.extend_from_slice(&chunk.data);
    }
    inst.stop().await.unwrap();
    data
}

async fn logic(bench: &SimBench, samples: u64) -> Vec<u8> {
    capture(
        bench,
        SimBench::logic_id(),
        AcqConfig {
            rate_hz: RATE,
            channels: (0..8).collect(),
            samples,
            trigger: None,
            ranges_v: vec![],
            continuous: false,
        },
    )
    .await
}

fn transitions(samples: &[u8]) -> Vec<Transition> {
    let mut out = vec![Transition {
        sample_index: 0,
        value: samples[0],
    }];
    for (i, w) in samples.windows(2).enumerate() {
        if w[0] != w[1] {
            out.push(Transition {
                sample_index: i as u64 + 1,
                value: w[1],
            });
        }
    }
    out
}

fn decode(mut decoder: impl Decoder, samples: &[u8]) -> Vec<AnnotationKind> {
    decoder.seed(samples[0]);
    let mut kinds: Vec<AnnotationKind> = decoder
        .feed(&transitions(samples), samples.len() as u64)
        .into_iter()
        .map(|a| a.kind)
        .collect();
    kinds.extend(
        decoder
            .finish(samples.len() as u64)
            .into_iter()
            .map(|a| a.kind),
    );
    kinds
}

fn uart_text(samples: &[u8], bit: u8) -> String {
    let decoder = UartDecoder::new(UartConfig {
        bit,
        sample_rate_hz: RATE,
        baud: 115_200,
        data_bits: 8,
        parity: Parity::None,
        stop_bits: StopBits::One,
        bit_order: BitOrder::Lsb,
        invert: false,
    })
    .unwrap();
    decode(decoder, samples)
        .into_iter()
        .filter_map(|k| match k {
            AnnotationKind::Data(b) => Some(b as u8 as char),
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn the_boot_traffic_decodes_on_the_logic_analyzer() {
    let bench = bench();
    // Power the board up while the analyzer is already running, to catch
    // the boot from its first edge.
    bench.set_port_power(1, false).unwrap();
    let cap = tokio::spawn({
        let bench = bench.clone();
        // Unshuffled: TX, RX, SCL, SDA, SCK, MOSI, CS, LED on D0..D7.
        async move { logic(&bench, RATE * 250 / 1000).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    bench.set_port_power(1, true).unwrap();
    let samples = cap.await.unwrap();

    let text = uart_text(&samples, 0);
    assert!(text.contains("AWIC trainer board"), "{text:?}");

    let i2c = decode(
        I2cDecoder::new(I2cConfig { scl: 2, sda: 3 }).unwrap(),
        &samples,
    );
    assert!(
        i2c.contains(&AnnotationKind::Address {
            address: 0x48,
            read: true
        }),
        "{i2c:?}"
    );

    let spi = decode(
        SpiDecoder::new(SpiConfig {
            clk: 4,
            mosi: Some(5),
            miso: None,
            cs: Some(6),
            cs_active_high: false,
            mode: SpiMode::Mode0,
            bit_order: SpiBitOrder::Msb,
            word_size: 8,
        })
        .unwrap(),
        &samples,
    );
    assert!(
        spi.contains(&AnnotationKind::Word {
            mosi: Some(0x9F),
            miso: None
        }),
        "{spi:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_command_typed_on_the_console_crosses_both_lines() {
    let bench = bench();
    bench.uart_open(115_200, 8, awic_sim::Parity::None, 1);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let cap = tokio::spawn({
        let bench = bench.clone();
        async move { logic(&bench, RATE * 60 / 1000).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    assert!(bench.uart_write(b"version\r"));
    let samples = cap.await.unwrap();
    assert!(uart_text(&samples, 1).contains("version\r"));
    assert!(uart_text(&samples, 0).contains("v1.4.2"));
}

#[tokio::test(start_paused = true)]
async fn the_counter_outputs_show_the_digit() {
    let bench = bench();
    bench.operate("KEY2", None, 1).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    bench.operate("KEY1", None, 6).unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let samples = capture(
        &bench,
        SimBench::logic2_id(),
        AcqConfig {
            rate_hz: 1_000_000,
            channels: (0..8).collect(),
            samples: 1_000,
            trigger: None,
            ranges_v: vec![],
            continuous: false,
        },
    )
    .await;
    // Pins 5, 6, 7, 9, 10, 11, 12, 13 = CO, f, g, d, a, e, b, c. A 6 on a
    // CD4026: c d e f g lit, a dark (no tail), carry low (count >= 5).
    let byte = samples[500];
    assert_eq!(byte, 0b1010_1110, "{byte:08b}");
    assert_eq!(bench.status().counter.display, "cdefg");
}

#[tokio::test(start_paused = true)]
async fn the_scope_sees_the_555() {
    let bench = bench();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let samples = capture(
        &bench,
        SimBench::scope_id(),
        AcqConfig {
            rate_hz: 1_000_000,
            channels: vec![0, 1],
            samples: 10_000,
            trigger: None,
            ranges_v: vec![5.0, 5.0],
            continuous: false,
        },
    )
    .await;
    let volts = |raw: u8| f64::from(raw) * (10.0 / 255.0) - 5.0;
    let cap: Vec<f64> = samples.iter().step_by(2).map(|&r| volts(r)).collect();
    let out: Vec<f64> = samples
        .iter()
        .skip(1)
        .step_by(2)
        .map(|&r| volts(r))
        .collect();
    let (lo, hi) = cap
        .iter()
        .fold((f64::MAX, f64::MIN), |(a, b), &v| (a.min(v), b.max(v)));
    assert!((lo - 5.0 / 3.0).abs() < 0.15, "cap min {lo}");
    assert!((hi - 10.0 / 3.0).abs() < 0.15, "cap max {hi}");
    let rises = out.windows(2).filter(|w| w[0] < 2.5 && w[1] >= 2.5).count();
    assert!((9..=11).contains(&rises), "{rises} rising edges in 10 ms");
}

/// Which channel each net landed on in a shuffled wiring.
fn channel_of(bench: &SimBench, net: awic_sim::world::Net) -> u8 {
    bench.wiring().logic.iter().position(|&n| n == net).unwrap() as u8
}

/// Whatever order the probes went on in, the channel survey finds the
/// console and the sensor bus on the right channels — the very job a user
/// of a shuffled bench gives it.
#[tokio::test(start_paused = true)]
async fn the_survey_untangles_any_shuffled_wiring() {
    use awic_decode::survey::{Guess, Surveyor};
    use awic_sim::world::Net;
    for seed in [3, 17, 99, 2026] {
        let bench = SimBench::new(&SimSettings {
            seed,
            faults: vec![],
            shuffle: true,
        })
        .unwrap();
        // Long enough for a status line (every 2 s) and a few sensor polls.
        let rate = 2_000_000;
        let samples = capture(
            &bench,
            SimBench::logic_id(),
            AcqConfig {
                rate_hz: rate,
                channels: (0..8).collect(),
                samples: rate * 5 / 2,
                trigger: None,
                ranges_v: vec![],
                continuous: false,
            },
        )
        .await;
        let mut surveyor = Surveyor::new(&[0, 1, 2, 3, 4, 5, 6, 7], rate, 0, samples[0]);
        surveyor.push(&transitions(&samples));
        let survey = surveyor.finish(samples.len() as u64);
        let tx = channel_of(&bench, Net::Tx);
        let (scl, sda) = (channel_of(&bench, Net::Scl), channel_of(&bench, Net::Sda));
        assert!(
            survey.guesses.iter().any(|g| matches!(g,
                Guess::Uart { channel, estimate, .. } if *channel == tx && estimate.baud == 115_200)),
            "seed {seed}: no UART on D{tx}: {:?}",
            survey.guesses
        );
        assert!(
            survey.guesses.iter().any(|g| matches!(g,
                Guess::I2c { scl: c, sda: d, .. } if *c == scl && *d == sda)),
            "seed {seed}: no I2C on D{scl}/D{sda}: {:?}",
            survey.guesses
        );
    }
}

/// The `baud_drift` fault is measurable: the baud estimator reads the
/// console faster than 115200 by as much as the MCU's clock has drifted.
#[tokio::test(start_paused = true)]
async fn a_drifting_console_measures_fast() {
    let bench = SimBench::new(&SimSettings {
        seed: 4,
        faults: vec!["baud_drift".into()],
        shuffle: false,
    })
    .unwrap();
    // 22.5 s into the 90 s cycle the drift peaks at +6.5 %.
    tokio::time::sleep(std::time::Duration::from_secs(21)).await;
    let samples = logic(&bench, RATE * 5 / 2).await;
    let estimate =
        awic_decode::baud::estimate_baud(&transitions(&samples), 0, RATE).expect("a baud rate");
    let drift = estimate.measured / 115_200.0 - 1.0;
    assert!(
        (0.055..0.075).contains(&drift),
        "measured {} baud, drift {:.3}",
        estimate.measured,
        drift
    );
}
