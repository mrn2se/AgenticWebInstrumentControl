//! Starting a second acquisition on a device that is still streaming (live
//! test, 2026-09-16): pressing "Single" again before the last one finished.
//!
//! The drivers already refuse to share one bulk endpoint — `start()` stops a
//! running acquisition first. What was missing was the server's own
//! bookkeeping: the superseded stream stayed registered, so when its task
//! ended with fewer samples than asked for, it was announced as
//! `acq_error: capture ended early: 0 of 1024 samples; the device stopped
//! streaming` — blaming the instrument for what the user had just asked for.

use std::sync::Arc;
use std::time::Duration;

use awic_core::Config;
use awic_fake::FakeBackend;
use awic_proto::{AcqConfig, AcqState, Event, Request, ResponseResult};
use awic_server::backend::PlaceholderBackend;
use awic_server::dispatch::handle_request;
use awic_server::state::AppState;
use awic_server::usb::UnimplementedUsbController;
use awic_server::ws::WsOut;
use tokio::sync::mpsc;

fn out() -> mpsc::Sender<WsOut> {
    mpsc::channel(64).0
}

async fn fake_state() -> (AppState, String) {
    let state = AppState::new(
        Config::default(),
        Arc::new(FakeBackend::new()),
        Arc::new(UnimplementedUsbController),
    );
    state.bootstrap().await;
    let device = state
        .registry()
        .list()
        .into_iter()
        .find(|d| {
            d.capabilities
                .iter()
                .any(|c| matches!(c, awic_proto::CapabilityInfo::Analog { .. }))
        })
        .expect("the fake backend always reports an analog device")
        .id;
    (state, device)
}

async fn configure(state: &AppState, device: &str, samples: u64) {
    let response = handle_request(
        state,
        &out(),
        0,
        Request::AcqConfigure {
            v: 1,
            id: 1,
            device: device.to_string(),
            cfg: AcqConfig {
                rate_hz: 1_000_000,
                channels: vec![0],
                samples,
                trigger: None,
                ranges_v: vec![],
                continuous: false,
            },
        },
    )
    .await;
    assert!(response.ok, "{:?}", response.error);
}

async fn start(state: &AppState, device: &str, id: u32) -> u16 {
    let response = handle_request(
        state,
        &out(),
        0,
        Request::AcqStart {
            v: 1,
            id,
            device: device.to_string(),
        },
    )
    .await;
    assert!(response.ok, "{:?}", response.error);
    let Some(ResponseResult::AcqStart(result)) = response.result else {
        panic!("expected an acq.start result");
    };
    result.stream
}

#[tokio::test]
async fn a_second_start_supersedes_the_first_without_blaming_the_device() {
    let (state, device) = fake_state().await;
    let mut events = state.event_bus().subscribe();
    // A second of samples: long enough that the second start lands while the
    // first is still streaming.
    configure(&state, &device, 1_000_000).await;

    let first = start(&state, &device, 2).await;
    tokio::time::sleep(Duration::from_millis(60)).await;
    let second = start(&state, &device, 3).await;
    assert_ne!(first, second);

    // The superseded stream must end as "stopped", with a reason that says
    // who ended it — never AcqState::Error.
    let ended = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(Event::AcqState {
                stream, state, msg, ..
            }) = events.recv().await
            {
                if stream == first && state != AcqState::Running && state != AcqState::Armed {
                    return (state, msg);
                }
            }
        }
    })
    .await
    .expect("the superseded stream must announce an end");
    assert_eq!(ended.0, AcqState::Stopped, "{ended:?}");
    let msg = ended.1.unwrap_or_default();
    assert!(msg.contains("stopped by request"), "{msg:?}");

    // And the device is left streaming exactly one acquisition: the new one
    // runs to completion.
    let done = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(Event::AcqState {
                stream, state, msg, ..
            }) = events.recv().await
            {
                if stream == second && matches!(state, AcqState::Stopped | AcqState::Error) {
                    return (state, msg);
                }
            }
        }
    })
    .await
    .expect("the new stream must finish");
    assert_eq!(done.0, AcqState::Stopped, "{done:?}");
    assert_eq!(done.1.as_deref(), Some("complete"), "{done:?}");
}

#[tokio::test]
async fn stopping_a_stream_that_was_already_superseded_is_a_named_error() {
    let (state, device) = fake_state().await;
    configure(&state, &device, 1_000_000).await;
    let first = start(&state, &device, 2).await;
    let _second = start(&state, &device, 3).await;

    // The client may still hold the old stream id; saying "no active stream"
    // is right, and must not touch the running one.
    let response = handle_request(
        &state,
        &out(),
        0,
        Request::AcqStop {
            v: 1,
            id: 4,
            stream: first,
        },
    )
    .await;
    assert!(!response.ok);
    assert_eq!(response.error.unwrap().code, "stream_not_found");
}

/// The placeholder backend has no instruments: `acq.start` on a device that
/// was never configured must still be a named error, not a panic in the new
/// supersede path.
#[tokio::test]
async fn starting_an_unconfigured_device_is_still_a_named_error() {
    let state = AppState::new(
        Config::default(),
        Arc::new(PlaceholderBackend),
        Arc::new(UnimplementedUsbController),
    );
    let response = handle_request(
        &state,
        &out(),
        0,
        Request::AcqStart {
            v: 1,
            id: 1,
            device: "1-1".into(),
        },
    )
    .await;
    assert!(!response.ok);
    assert_eq!(response.error.unwrap().code, "device_not_configured");
}
