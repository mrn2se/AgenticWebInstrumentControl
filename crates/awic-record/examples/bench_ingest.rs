//! Ingest throughput and compression ratio report for `awic-record`
//! (SPECIFICATION.md §2.8, DEVELOPMENT.md risk R11).
//!
//! Run in release mode for meaningful numbers:
//! `cargo run --release -p awic-record --example bench_ingest`

use awic_record::analog::AnalogStore;
use awic_record::{Recording, RecordingKind, RecordingSpec};
use std::time::Instant;

/// A simple, fast, deterministic PRNG (xorshift64*) — no need for the `rand`
/// crate in a dependency-minimal pure crate.
struct Xorshift(u64);
impl Xorshift {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn next_u8(&mut self) -> u8 {
        (self.next_u64() >> 56) as u8
    }
    /// Approximately standard-normal via a cheap sum-of-uniforms.
    fn next_gaussish(&mut self) -> f64 {
        let mut sum = 0.0;
        for _ in 0..4 {
            sum += (self.next_u64() >> 40) as f64 / (1u64 << 24) as f64;
        }
        (sum - 2.0) * 1.6 // roughly unit variance
    }
}

/// 8-channel packed digital signal where each channel is a slow square wave
/// (realistic logic-analyzer traffic: long runs, occasional edges) — channel
/// `ch` toggles at `base_hz * (ch + 1)`.
fn realistic_digital(n: usize, rate_hz: u64, base_hz: f64, start_sample: u64) -> Vec<u8> {
    let half_periods: Vec<u64> = (0..8u8)
        .map(|ch| ((rate_hz as f64 / (2.0 * base_hz * (ch as f64 + 1.0))).max(1.0)) as u64)
        .collect();
    (0..n)
        .map(|i| {
            let s = start_sample + i as u64;
            let mut byte = 0u8;
            for (ch, &hp) in half_periods.iter().enumerate() {
                if (s / hp) % 2 == 1 {
                    byte |= 1 << ch;
                }
            }
            byte
        })
        .collect()
}

fn random_digital(n: usize, rng: &mut Xorshift) -> Vec<u8> {
    (0..n).map(|_| rng.next_u8()).collect()
}

/// D0 only, a clean 1 kHz square wave sampled at 1 MHz.
fn square_wave_d0(n: usize, rate_hz: u64, freq_hz: f64) -> Vec<u8> {
    let half_period = (rate_hz as f64 / (2.0 * freq_hz)) as u64;
    (0..n)
        .map(|i| {
            if (i as u64 / half_period) % 2 == 1 {
                1u8
            } else {
                0u8
            }
        })
        .collect()
}

/// A noisy analog rendition of the same 1 kHz square wave, u8 codes, +/-3
/// code Gaussian-ish noise, biased away from the rails so noise doesn't clip.
fn noisy_square_u8(n: usize, rate_hz: u64, freq_hz: f64, rng: &mut Xorshift) -> Vec<u8> {
    let half_period = (rate_hz as f64 / (2.0 * freq_hz)) as u64;
    (0..n)
        .map(|i| {
            let level = if (i as u64 / half_period) % 2 == 1 {
                180.0
            } else {
                60.0
            };
            let noisy = level + rng.next_gaussish() * 3.0;
            noisy.round().clamp(0.0, 255.0) as u8
        })
        .collect()
}

fn append_digital_chunks(rec: &mut Recording, data: &[u8], chunk: usize) -> f64 {
    let start = Instant::now();
    for c in data.chunks(chunk) {
        rec.append_packed(c).unwrap();
    }
    start.elapsed().as_secs_f64()
}

fn append_analog_chunks(rec: &mut Recording, data: &[u8], chunk: usize) -> f64 {
    let start = Instant::now();
    for c in data.chunks(chunk) {
        rec.append_analog(c).unwrap();
    }
    start.elapsed().as_secs_f64()
}

fn main() {
    let mut rng = Xorshift(0xC0FFEE_u64.wrapping_mul(2654435761).wrapping_add(1));

    println!("=== Digital ingest throughput ===");
    {
        let rate_hz = 24_000_000u64;
        let seconds = 0.5;
        let n = (rate_hz as f64 * seconds) as usize;
        let data = realistic_digital(n, rate_hz, 1000.0, 0);
        let mut rec = Recording::new(RecordingSpec {
            kind: RecordingKind::Digital,
            rate_hz,
        });
        let elapsed = append_digital_chunks(&mut rec, &data, 65536);
        let real_time = n as f64 / rate_hz as f64;
        println!(
            "realistic 8ch @ {} MS/s: {:.3}s for {:.3}s of signal -> {:.1}x real time, {:.0} Msamples/s",
            rate_hz / 1_000_000,
            elapsed,
            real_time,
            real_time / elapsed,
            n as f64 / elapsed / 1e6
        );
    }
    {
        let rate_hz = 24_000_000u64;
        let seconds = 0.2;
        let n = (rate_hz as f64 * seconds) as usize;
        let data = random_digital(n, &mut rng);
        let mut rec = Recording::new(RecordingSpec {
            kind: RecordingKind::Digital,
            rate_hz,
        });
        let elapsed = append_digital_chunks(&mut rec, &data, 65536);
        let real_time = n as f64 / rate_hz as f64;
        println!(
            "worst-case random 8ch @ {} MS/s: {:.3}s -> {:.2}x real time, {:.0} Msamples/s",
            rate_hz / 1_000_000,
            elapsed,
            real_time / elapsed,
            n as f64 / elapsed / 1e6
        );
    }

    println!("\n=== Analog ingest throughput (FX2 oscilloscope: 2ch u8 @ 16 MS/s aggregate) ===");
    {
        let rate_hz = 16_000_000u64 / 2; // per-channel rate for a 2-channel 16 MS/s aggregate capture
        let seconds = 1.0;
        let n = (rate_hz as f64 * seconds) as usize;
        let ch0 = noisy_square_u8(n, rate_hz, 1000.0, &mut rng);
        let mut data = Vec::with_capacity(n * 2);
        for &c in &ch0 {
            data.push(c);
            data.push(rng.next_u8());
        }

        let mut rec = Recording::new(RecordingSpec {
            kind: RecordingKind::Analog {
                channel_count: 2,
                sample_bits: 8,
            },
            rate_hz,
        });
        let elapsed = append_analog_chunks(&mut rec, &data, 65536 * 2);
        let real_time = n as f64 / rate_hz as f64;
        let target_samples_per_sec = 32_000_000.0; // 2ch * 16 MS/s
        let achieved = (n as f64 * 2.0) / elapsed;
        println!(
            "via Recording (store + summary tree): 2ch u8 @ {:.1} MS/s/ch: {:.3}s -> {:.1}x real time, {:.0} Msamples/s aggregate ({:.2}x the 32 Msamples/s target)",
            rate_hz as f64 / 1e6,
            elapsed,
            real_time / elapsed,
            achieved / 1e6,
            achieved / target_samples_per_sec
        );

        // Isolate the store's own ingest cost (no summary tree at all) to
        // see whether the tree or the codec/de-interleave path dominates.
        let mut store = AnalogStore::new(2, 8);
        let start = Instant::now();
        for c in data.chunks(65536 * 2) {
            store.append_analog(c).unwrap();
        }
        let store_elapsed = start.elapsed().as_secs_f64();
        let store_achieved = (n as f64 * 2.0) / store_elapsed;
        println!(
            "via AnalogStore alone (no summary tree): {:.3}s -> {:.0} Msamples/s aggregate ({:.2}x the 32 Msamples/s target)",
            store_elapsed,
            store_achieved / 1e6,
            store_achieved / target_samples_per_sec
        );
    }

    println!("\n=== Compression ratios ===");
    {
        let rate_hz = 1_000_000u64;
        let n = rate_hz as usize; // 1 second
        let data = square_wave_d0(n, rate_hz, 1000.0);
        let mut rec = Recording::new(RecordingSpec {
            kind: RecordingKind::Digital,
            rate_hz,
        });
        rec.append_packed(&data).unwrap();
        let raw = rec.raw_bytes(0, rec.head_sample());
        let used = rec.bytes();
        println!(
            "digital 1kHz square @ 1 MHz on D0, 1s: raw {} bytes, stored ~{} bytes, ratio {:.1}x",
            raw,
            used,
            raw as f64 / used as f64
        );
    }
    {
        let rate_hz = 1_000_000u64;
        let n = rate_hz as usize;
        let data = noisy_square_u8(n, rate_hz, 1000.0, &mut rng);
        let mut rec = Recording::new(RecordingSpec {
            kind: RecordingKind::Analog {
                channel_count: 1,
                sample_bits: 8,
            },
            rate_hz,
        });
        rec.append_analog(&data).unwrap();
        let raw = rec.raw_bytes(0, rec.head_sample());
        let used = rec.bytes();
        println!(
            "analog noisy 1kHz square (+/-3 codes) @ 1 MHz u8, 1s: raw {} bytes, stored ~{} bytes, ratio {:.2}x",
            raw,
            used,
            raw as f64 / used as f64
        );
    }

    println!("\n=== Digital summary-tree memory (M12 exit criterion) ===");
    {
        // The CAL square: 1 kHz on D0 @ 1 MHz, but for 600 s instead of 1 s
        // — streamed in chunks rather than materialized whole (600 MS at
        // u8 would be a 600 MB input buffer, which is a benchmark-harness
        // problem, not a `Recording` one).
        let rate_hz = 1_000_000u64;
        let seconds = 600u64;
        let total = rate_hz * seconds;
        let half_period = (rate_hz as f64 / (2.0 * 1000.0)) as u64;
        let mut rec = Recording::new(RecordingSpec {
            kind: RecordingKind::Digital,
            rate_hz,
        });
        let chunk = 1_000_000usize;
        let mut buf = vec![0u8; chunk];
        let mut produced = 0u64;
        while produced < total {
            let n = chunk.min((total - produced) as usize);
            for (i, slot) in buf.iter_mut().take(n).enumerate() {
                let s = produced + i as u64;
                *slot = if (s / half_period) % 2 == 1 { 1 } else { 0 };
            }
            rec.append_packed(&buf[..n]).unwrap();
            produced += n as u64;
        }
        let raw = rec.raw_bytes(0, rec.head_sample());
        let used = rec.bytes();
        let verdict = if used < 10_000_000 {
            "[under 10 MB target]"
        } else {
            "[OVER 10 MB TARGET]"
        };
        println!(
            "1 kHz square on D0 @ 1 MS/s, {seconds}s (CAL-like, sparse tree): raw {raw} bytes, stored ~{used} bytes (~{:.2} MB), ratio {:.1}x {verdict}",
            used as f64 / 1e6,
            raw as f64 / used as f64,
        );
    }
    {
        // Dense case: every one of the 8 lanes toggling, for contrast —
        // the sparse encoding's per-bucket cost degrades gracefully here
        // (falls back to per-channel entries) but isn't the target case.
        let rate_hz = 1_000_000u64;
        let seconds = 60u64;
        let n = (rate_hz * seconds) as usize;
        let data = realistic_digital(n, rate_hz, 20_000.0, 0); // ch7 toggles at 160 kHz
        let mut rec = Recording::new(RecordingSpec {
            kind: RecordingKind::Digital,
            rate_hz,
        });
        for c in data.chunks(1_000_000) {
            rec.append_packed(c).unwrap();
        }
        let raw = rec.raw_bytes(0, rec.head_sample());
        let used = rec.bytes();
        println!(
            "dense 8-channel (up to 160 kHz) @ 1 MS/s, {seconds}s: raw {raw} bytes, stored ~{used} bytes (~{:.2} MB), ratio {:.2}x",
            used as f64 / 1e6,
            raw as f64 / used as f64,
        );
    }
}
