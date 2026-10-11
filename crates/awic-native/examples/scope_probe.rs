//! Bench diagnostic: capture the CAL square wave on CH1 of the FX2 oscilloscope
//! across the rate ladder and check the timebase two independent ways — the
//! driver's completion-timing rate check, and the CAL frequency in samples.
//! Run as root: `sudo -n ./target/debug/examples/scope_probe`.

use std::sync::Arc;

use awic_core::{AcqConfig, DeviceId, Instrument, SampleSink};
use awic_native::fx2scope::Fx2ScopeInstrument;
use awic_native::FirmwareResolver;

/// CAL frequency from rising crossings of the midpoint between the two
/// levels, averaged over complete periods.
fn cal_frequency(codes: &[u8], rate_hz: u64) -> Option<f64> {
    let (lo, hi) = (*codes.iter().min()? as f64, *codes.iter().max()? as f64);
    let (mid, band) = ((lo + hi) / 2.0, (hi - lo) * 0.1);
    let (mut high, mut rises) = (codes[0] as f64 > mid, Vec::new());
    for (i, &c) in codes.iter().enumerate() {
        let c = c as f64;
        if !high && c > mid + band {
            high = true;
            rises.push(i);
        } else if high && c < mid - band {
            high = false;
        }
    }
    (rises.len() >= 3).then(|| {
        let periods = (rises.len() - 1) as f64;
        rate_hz as f64 * periods / (rises[rises.len() - 1] - rises[0]) as f64
    })
}

/// (shortest - mean, longest - mean) in samples. Lost samples only ever
/// shorten a period; source jitter moves both ways.
fn period_extremes(codes: &[u8]) -> Option<(f64, f64)> {
    let (lo, hi) = (*codes.iter().min()? as f64, *codes.iter().max()? as f64);
    let (mid, band) = ((lo + hi) / 2.0, (hi - lo) * 0.1);
    let (mut high, mut rises) = (codes[0] as f64 > mid, Vec::new());
    for (i, &c) in codes.iter().enumerate() {
        let c = c as f64;
        if !high && c > mid + band {
            high = true;
            rises.push(i);
        } else if high && c < mid - band {
            high = false;
        }
    }
    let periods: Vec<f64> = rises.windows(2).map(|w| (w[1] - w[0]) as f64).collect();
    let mean = periods.iter().sum::<f64>() / periods.len().max(1) as f64;
    let min = periods.iter().cloned().fold(f64::MAX, f64::min);
    let max = periods.iter().cloned().fold(f64::MIN, f64::max);
    let at = periods.iter().position(|&p| p == min).unwrap_or(0);
    eprintln!(
        "    shortest period is #{at} of {}, starting at sample {} ({:.0} us into the capture)",
        periods.len(),
        rises.get(at).copied().unwrap_or(0),
        rises.get(at).copied().unwrap_or(0) as f64 / (mean / 1000.0)
    );
    Some((min - mean, max - mean))
}

#[tokio::main]
async fn main() {
    let id = DeviceId::new(3, vec![3, 4, 2], 0, 0);
    let mut scope = Fx2ScopeInstrument::new(id, Arc::new(FirmwareResolver::default()));
    scope.prepare().await.expect("prepare");
    println!(
        "{:>9}  {:>9} {:>6} {:>12} {:>9}  {:>10}",
        "rate", "samples", "stall", "drv measured", "mismatch", "CAL freq"
    );
    for rate in [24_000_000u64, 24_000_000, 30_000_000, 48_000_000] {
        let samples = rate; // 1 s — long enough to cross many transfer boundaries
        let cfg = AcqConfig {
            rate_hz: rate,
            channels: vec![0],
            samples,
            trigger: None,
            ranges_v: vec![0.5],
            continuous: false,
        };
        if let Err(e) = scope.configure(cfg).await {
            println!("{rate:>9}  refused: {e}");
            continue;
        }
        let (sink, mut rx) = SampleSink::channel(256);
        scope.start(sink).await.expect("start");
        let mut raw = Vec::new();
        while let Some(chunk) = rx.recv().await {
            raw.extend_from_slice(&chunk.data);
        }
        scope.stop().await.ok();
        let st = scope.last_stats();
        let f = cal_frequency(&raw, rate).map_or("n/a".into(), |f| format!("{f:.2} Hz"));
        println!(
            "{rate:>9}  {:>9} {:>6} {:>12} {:>9}  {:>10} {:>14}",
            raw.len(),
            st.stalled,
            st.measured_rate_hz
                .map_or("n/a".into(), |m| format!("{m:.0}")),
            st.rate_mismatch,
            f,
            period_extremes(&raw).map_or("n/a".into(), |(s, l)| format!(
                "short {s:+.0} long {l:+.0} (of {})",
                rate / 1000
            ))
        );
    }
}
