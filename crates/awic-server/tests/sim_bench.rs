//! The virtual bench inside the server: its instruments follow the virtual
//! hub's port power through the same hot-plug path as hardware, its console
//! comes and goes from `term.sources`, and `sim.*` answers — or is refused
//! on a server without a bench.

use std::sync::Arc;
use std::time::Duration;

use awic_core::{Backend, CompositeBackend, Config};
use awic_proto::{DeviceState, Request, ResponseResult};
use awic_server::dispatch::handle_request;
use awic_server::sim::{SimSerialProvider, SimUsbController};
use awic_server::terminal::TerminalSourceProvider;
use awic_server::usb::{UnimplementedUsbController, UsbController};
use awic_server::AppState;
use awic_sim::{SimBackend, SimBench, SimSettings};

fn bench_state() -> (AppState, SimBench) {
    let bench = SimBench::new(&SimSettings {
        seed: 7,
        faults: vec![],
        shuffle: true,
    })
    .unwrap();
    let backends: Vec<Arc<dyn Backend>> = vec![Arc::new(SimBackend::new(bench.clone()))];
    let usb: Arc<dyn UsbController> = Arc::new(SimUsbController::new(
        Arc::new(UnimplementedUsbController),
        bench.clone(),
    ));
    let providers: Vec<Arc<dyn TerminalSourceProvider>> =
        vec![Arc::new(SimSerialProvider::new(bench.clone()))];
    let state = AppState::with_terminal_providers(
        Config::default(),
        Arc::new(CompositeBackend::new(backends)),
        usb,
        providers,
    );
    state.set_sim(bench.clone());
    tokio::spawn(awic_server::sim::forward_changes(
        bench.clone(),
        state.event_bus().clone(),
    ));
    (state, bench)
}

async fn until(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..100 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn instruments_follow_the_virtual_hub() {
    let (state, _bench) = bench_state();
    state.bootstrap().await;
    tokio::spawn(awic_server::state::run_instrument_hotplug(state.clone()));
    let logic = SimBench::logic_id();
    assert_eq!(state.registry().state_of(&logic), Some(DeviceState::Ready));

    state.usb().power("254-1", 2, false).await.unwrap();
    until("the analyzer to leave", || {
        !state.registry().contains(&logic)
    })
    .await;

    state.usb().power("254-1", 2, true).await.unwrap();
    until("the analyzer to come back ready", || {
        state.registry().state_of(&logic) == Some(DeviceState::Ready)
    })
    .await;
}

#[tokio::test]
async fn the_console_leaves_term_sources_with_the_board_power() {
    let (state, _bench) = bench_state();
    let sources = || state.terminal().sources();
    assert!(sources()
        .iter()
        .any(|s| s.source == SimBench::serial_source()));
    state.usb().power("254-1", 1, false).await.unwrap();
    assert!(!sources()
        .iter()
        .any(|s| s.source == SimBench::serial_source()));
}

#[tokio::test]
async fn sim_requests_answer_on_a_bench_and_are_refused_without_one() {
    let (state, _bench) = bench_state();
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let resp = handle_request(
        &state,
        &tx,
        1,
        Request::SimSet {
            v: 1,
            id: 1,
            faults: Some(vec!["osc_jitter".into()]),
            mystery: None,
            reveal: None,
            osc_r2_ohm: Some(10_000.0),
            input: None,
            state: None,
            presses: None,
            guess: None,
        },
    )
    .await;
    let Some(ResponseResult::SimStatus(status)) = resp.result else {
        panic!("{:?}", resp.error);
    };
    assert_eq!(status.osc_r2_ohm, 10_000.0);
    assert!(status
        .faults
        .iter()
        .any(|f| f.id == "osc_jitter" && f.active == Some(true)));

    let bad = handle_request(
        &state,
        &tx,
        1,
        Request::SimSet {
            v: 1,
            id: 2,
            faults: Some(vec!["gremlins".into()]),
            mystery: None,
            reveal: None,
            osc_r2_ohm: None,
            input: None,
            state: None,
            presses: None,
            guess: None,
        },
    )
    .await;
    assert_eq!(bad.error.unwrap().code, "invalid_params");

    let plain = AppState::placeholder(Config::default());
    let off = handle_request(&plain, &tx, 1, Request::SimStatus { v: 1, id: 3 }).await;
    assert_eq!(off.error.unwrap().code, "sim_disabled");
}
