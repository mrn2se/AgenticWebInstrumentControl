//! Bench diagnostic: how many stale timepoints survive the start-of-capture
//! FIFO flush? Alternates ±5 V and ±0.5 V so old and fresh samples fall in
//! disjoint code ranges (±5 V: 135..=141, ±0.5 V: >= 150), then counts the
//! leading samples that still carry the previous range's levels.

use std::sync::Arc;

use awic_core::{AcqConfig, DeviceId, Instrument, SampleSink};
use awic_native::fx2scope::Fx2ScopeInstrument;
use awic_native::FirmwareResolver;

#[tokio::main]
async fn main() {
    let rate: u64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(1_000_000);
    let id = DeviceId::new(3, vec![3, 4, 2], 0, 0);
    let mut scope = Fx2ScopeInstrument::new(id, Arc::new(FirmwareResolver::default()));
    scope.prepare().await.expect("prepare");
    let mut survivors = Vec::new();
    for i in 0..12 {
        let range = if i % 2 == 0 { 5.0f32 } else { 0.5 };
        scope
            .configure(AcqConfig {
                rate_hz: rate,
                channels: vec![0],
                samples: 50_000,
                trigger: None,
                ranges_v: vec![range],
                continuous: false,
            })
            .await
            .expect("configure");
        let (sink, mut rx) = SampleSink::channel(64);
        scope.start(sink).await.expect("start");
        let mut raw = Vec::new();
        while let Some(chunk) = rx.recv().await {
            raw.extend_from_slice(&chunk.data);
        }
        scope.stop().await.ok();
        if i == 0 {
            continue;
        } // no previous range to be stale from
        let stale = |c: u8| if range == 0.5 { c < 150 } else { c >= 150 };
        let n = raw.iter().take_while(|&&c| stale(c)).count();
        survivors.push(n);
        println!("capture {i:>2} (±{range} V): {n:>5} stale timepoints survived the flush; first 12: {:?}", &raw[..12]);
    }
    println!(
        "max surviving: {} timepoints = {} wire bytes beyond the flush",
        survivors.iter().max().unwrap(),
        2 * survivors.iter().max().unwrap()
    );
}
