//! `capture.render` over the dispatch path, and `GET /api/render` over HTTP
//! (SPECIFICATION.md §5.6).
//!
//! The drawing itself is `awic-render`'s to test. What only shows up here is
//! the wiring: that a capture handle resolves, that a refusal comes back as an
//! error a caller can act on rather than an empty picture, and that the
//! browser's endpoint serves the same document the wire op returns.

use std::sync::Arc;
use std::time::Duration;

use awic_core::Config;
use awic_fake::FakeBackend;
use awic_proto::{AcqConfig, RenderKind, Request, ResponseResult};
use awic_server::dispatch::handle_request;
use awic_server::state::AppState;
use awic_server::usb::UnimplementedUsbController;
use awic_server::ws::WsOut;
use tokio::sync::mpsc;

fn out() -> mpsc::Sender<WsOut> {
    mpsc::channel(64).0
}

/// A finished analog capture from the fake backend, and its handle.
async fn captured() -> (AppState, String) {
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
        .expect("the fake backend reports an analog device")
        .id;

    let response = handle_request(
        &state,
        &out(),
        0,
        Request::AcqConfigure {
            v: 1,
            id: 1,
            device: device.clone(),
            cfg: AcqConfig {
                rate_hz: 1_000_000,
                // Two channels: the eye view's one-channel rule can only be
                // reached on a capture that actually holds more than one.
                channels: vec![0, 1],
                samples: 200_000,
                trigger: None,
                ranges_v: vec![],
                continuous: false,
            },
        },
    )
    .await;
    assert!(response.ok, "{:?}", response.error);

    let response = handle_request(
        &state,
        &out(),
        0,
        Request::AcqStart {
            v: 1,
            id: 2,
            device,
        },
    )
    .await;
    assert!(response.ok, "{:?}", response.error);
    let Some(ResponseResult::AcqStart(result)) = response.result else {
        panic!("expected an acq.start result");
    };

    // The fake backend streams; wait for the capture to be filed.
    for _ in 0..100 {
        if let Ok(id) = result.capture.parse::<awic_core::CaptureId>() {
            if state.capture_store().get(&id).is_some() {
                return (state, result.capture);
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the capture never reached the store");
}

async fn render(
    state: &AppState,
    capture: &str,
    kind: RenderKind,
    channels: Vec<u8>,
) -> awic_proto::Response {
    handle_request(
        state,
        &out(),
        0,
        Request::CaptureRender {
            v: 1,
            id: 7,
            capture: capture.to_string(),
            kind,
            channels,
            width: None,
            height: None,
            t0_s: None,
            t1_s: None,
            period_s: None,
            interval_kind: None,
            tolerance: None,
            title: None,
        },
    )
    .await
}

#[tokio::test]
async fn a_waveform_render_answers_with_a_document_and_its_measurements() {
    let (state, capture) = captured().await;
    let response = render(&state, &capture, RenderKind::Waveform, vec![]).await;
    assert!(response.ok, "{:?}", response.error);
    let Some(ResponseResult::CaptureRender(result)) = response.result else {
        panic!("expected a capture.render result");
    };
    assert_eq!(result.capture, capture);
    assert_eq!(result.mime_type, "image/svg+xml");
    assert!(result.svg.starts_with("<svg xmlns="));
    assert!(result.svg.ends_with("</svg>"));
    assert!(
        !result.metrics.is_empty(),
        "a view reports what it measured"
    );
    // Every metric is labelled, and a quantity carries a machine-readable
    // value alongside the text the picture shows.
    for metric in &result.metrics {
        assert!(!metric.name.is_empty());
        assert!(!metric.text.is_empty(), "{} has no text", metric.name);
    }
    let window = result
        .metrics
        .iter()
        .find(|m| m.name == "window")
        .expect("a window metric");
    assert!(
        (window.value.expect("a number") - 0.2).abs() < 1e-6,
        "{window:?}"
    );
}

/// Findings have to survive the trip out: they are the part an agent acts on.
#[tokio::test]
async fn an_anomaly_digest_carries_its_findings_over_the_wire() {
    let (state, capture) = captured().await;
    let response = render(&state, &capture, RenderKind::Anomalies, vec![0]).await;
    assert!(response.ok, "{:?}", response.error);
    let Some(ResponseResult::CaptureRender(result)) = response.result else {
        panic!("expected a capture.render result");
    };
    assert_eq!(result.kind, RenderKind::Anomalies);
    // The fake backend's waveform is regular, so the interesting assertion is
    // that the shape arrives at all: a reference, a count, and a tolerance.
    for name in ["reference", "outliers", "tolerance", "intervals"] {
        assert!(
            result.metrics.iter().any(|m| m.name == name),
            "no {name} metric; have {:?}",
            result.metrics.iter().map(|m| &m.name).collect::<Vec<_>>()
        );
    }
    // Any finding that is located carries its time as a number, not prose.
    for finding in &result.findings {
        assert!(!finding.label.is_empty());
        assert!(!finding.detail.is_empty());
        if let Some(at) = finding.at_s {
            assert!(at.is_finite() && at >= 0.0, "{finding:?}");
        }
    }
}

#[tokio::test]
async fn a_histogram_names_its_populations_over_the_wire() {
    let (state, capture) = captured().await;
    let response = render(&state, &capture, RenderKind::Histogram, vec![0]).await;
    assert!(response.ok, "{:?}", response.error);
    let Some(ResponseResult::CaptureRender(result)) = response.result else {
        panic!("expected a capture.render result");
    };
    assert!(result.metrics.iter().any(|m| m.name == "populations"));
    assert!(
        result
            .metrics
            .iter()
            .any(|m| m.name == "population_1_length"),
        "at least one population is always named"
    );
    assert!(!result.findings.is_empty(), "each population is a finding");
}

#[tokio::test]
async fn an_unknown_capture_is_a_named_error_not_an_empty_picture() {
    let (state, _) = captured().await;
    let response = render(
        &state,
        "capture://00000000-0000-0000-0000-000000000000",
        RenderKind::Waveform,
        vec![],
    )
    .await;
    assert!(!response.ok);
    let error = response.error.expect("an error");
    assert_eq!(error.code, "capture_not_found");
}

#[tokio::test]
async fn a_malformed_handle_says_so_rather_than_reporting_it_missing() {
    let (state, _) = captured().await;
    let response = render(&state, "not-a-handle", RenderKind::Waveform, vec![]).await;
    assert!(!response.ok);
    let error = response.error.expect("an error");
    assert_eq!(error.code, "invalid_capture_id");
}

/// The renderer's refusals have to survive the trip out: they name what to do
/// differently, which is the whole value of refusing rather than drawing an
/// empty box.
#[tokio::test]
async fn a_refusal_carries_the_renderers_own_reason() {
    let (state, capture) = captured().await;
    let response = render(&state, &capture, RenderKind::Eye, vec![0, 1]).await;
    assert!(!response.ok);
    let error = response.error.expect("an error");
    assert_eq!(error.code, "cannot_render");
    assert!(error.msg.contains("folds one channel"), "{}", error.msg);
}

#[tokio::test]
async fn the_http_endpoint_serves_the_same_document_as_the_wire_op() {
    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode};
    use tower::ServiceExt;

    let (state, capture) = captured().await;
    let wire = render(&state, &capture, RenderKind::Waveform, vec![]).await;
    let Some(ResponseResult::CaptureRender(expected)) = wire.result else {
        panic!("expected a capture.render result");
    };

    let app = awic_server::http::router("does-not-exist").with_state(state);
    let response = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .uri(format!(
                    "/api/render?capture={}&kind=waveform",
                    urlencode(&capture)
                ))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("a response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("image/svg+xml; charset=utf-8")
    );
    let body = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("a body");
    assert_eq!(String::from_utf8_lossy(&body), expected.svg);

    // F-09: the same view as PNG.
    let response = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .uri(format!(
                    "/api/render?capture={}&kind=waveform&format=png",
                    urlencode(&capture)
                ))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("a response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("image/png")
    );
    let body = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("a body");
    assert!(body.starts_with(b"\x89PNG\r\n\x1a\n"), "not a PNG");

    // A capture that is not there is a 404 with a reason, not an empty 200.
    let response = app
        .oneshot(
            HttpRequest::builder()
                .uri("/api/render?capture=capture%3A%2F%2F00000000-0000-0000-0000-000000000000")
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("a response");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            ':' => "%3A".to_string(),
            '/' => "%2F".to_string(),
            other => other.to_string(),
        })
        .collect()
}

/// B-58: `/api/render` ignored `span_s`, which `render_view` over MCP takes, so
/// a request for a 320 µs window was served the whole capture; and a repeated
/// `channels=4&channels=6` kept only the last value. Both forms work now, and
/// a parameter this endpoint does not know is refused rather than ignored.
#[tokio::test]
async fn the_http_endpoint_takes_span_s_both_channel_forms_and_no_unknown_keys() {
    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode};
    use tower::ServiceExt;

    let (state, capture) = captured().await;
    let app = awic_server::http::router("does-not-exist").with_state(state);
    let encoded = urlencode(&capture);
    let get = |query: String| {
        let app = app.clone();
        let encoded = encoded.clone();
        async move {
            let response = app
                .oneshot(
                    HttpRequest::builder()
                        .uri(format!("/api/render?capture={encoded}&{query}"))
                        .body(Body::empty())
                        .expect("a request"),
                )
                .await
                .expect("a response");
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
                .await
                .expect("a body");
            (status, String::from_utf8_lossy(&body).to_string())
        }
    };

    // `span_s` draws the window it names: the view's own footer reports the
    // window it drew, so the document says whether it was honoured.
    let (status, svg) = get("kind=waveform&t0_s=0.0005&span_s=0.0002".to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        svg.contains("200 us window"),
        "span_s was ignored: {}",
        svg.lines().next().unwrap_or_default()
    );
    let (_, whole) = get("kind=waveform".to_string()).await;
    assert!(
        !whole.contains("200 us window"),
        "the whole capture is wider"
    );

    // Both spellings of a channel list name both channels.
    for query in [
        "kind=waveform&channels=0,1",
        "kind=waveform&channels=0&channels=1",
    ] {
        let (status, svg) = get(query.to_string()).await;
        assert_eq!(status, StatusCode::OK, "{query}");
        assert!(svg.contains(">CH1<"), "{query}: CH1 missing");
        assert!(svg.contains(">CH2<"), "{query}: CH2 missing");
    }

    // A channel that is not a number, and `span_s` with `t1_s`, are refused
    // with a reason rather than quietly dropped.
    let (status, body) = get("kind=waveform&channels=0,x".to_string()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("not a channel number"), "{body}");
    let (status, body) = get("kind=waveform&span_s=0.001&t1_s=0.002".to_string()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("not both"), "{body}");
    let (status, body) = get("kind=waveform&span_s=0".to_string()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("must be positive"), "{body}");

    // An unknown key is a 400 naming what this endpoint does take.
    let (status, body) = get("kind=waveform&spam_s=0.0002".to_string()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("spam_s") && body.contains("span_s"), "{body}");
}
