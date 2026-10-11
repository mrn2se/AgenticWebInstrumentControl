//! `GET /api/health`, `GET /api/render` and the static frontend
//! (SPECIFICATION.md §1.1, §5.6, §11).
//!
//! The built frontend (`vite build` output) lives in a directory built by a
//! separate task; this server must still start cleanly when it hasn't been
//! built yet, serving a clear placeholder instead of failing.

use std::path::Path;

use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use tower_http::services::{ServeDir, ServeFile};

use crate::state::AppState;

#[derive(Serialize)]
struct HealthResponse {
    version: &'static str,
    devices: usize,
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        version: env!("CARGO_PKG_VERSION"),
        devices: state.registry().len(),
    })
}

/// `GET /api/render?capture=…&kind=eye&channels=0&width=…` — the same view
/// `capture.render` returns, served as an SVG document (or, with
/// `format=png`, the same picture as PNG, F-09).
///
/// This exists so the browser's snapshot button is an `<img>`/download away
/// rather than a WebSocket round trip with a blob to assemble, and so a view
/// can be linked to from an issue or a chat. It is a plain GET of a capture
/// the caller already has a handle for, so it carries no more authority than
/// the handle does.
/// Every query key this endpoint understands.
///
/// An unrecognised one is refused rather than ignored. `span_s` used to be
/// ignored here while `render_view` over MCP took it, so a request that asked
/// for a 320 us window was served the whole capture and looked like a renderer
/// bug (B-58); a mistyped key has exactly the same shape.
const RENDER_KEYS: [&str; 13] = [
    "capture",
    "kind",
    "channels",
    "width",
    "height",
    "t0_s",
    "t1_s",
    "span_s",
    "period_s",
    "interval_kind",
    "tolerance",
    "title",
    "format",
];

/// The query string as pairs, keeping repeats.
///
/// `channels=4,6` and `channels=4&channels=6` mean the same thing and both
/// work; a `HashMap` kept only the last value of a repeated key, so the
/// repeated form silently drew one channel (B-58).
struct RenderParams(Vec<(String, String)>);

impl RenderParams {
    /// The last value given for `key` — a repeated scalar is a mistake, and the
    /// last one is what a reader of the URL would expect to win.
    fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn all<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a str> {
        self.0
            .iter()
            .filter(move |(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn unknown(&self) -> Option<&str> {
        self.0
            .iter()
            .map(|(k, _)| k.as_str())
            .find(|k| !RENDER_KEYS.contains(k))
    }
}

async fn render_view(
    State(state): State<AppState>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    let params = RenderParams(pairs);
    if let Some(unknown) = params.unknown() {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "no parameter {unknown:?}; this endpoint takes {}",
                RENDER_KEYS.join(", ")
            ),
        )
            .into_response();
    }
    let Some(handle) = params.get("capture") else {
        return (StatusCode::BAD_REQUEST, "capture= is required").into_response();
    };
    let kind = match params.get("kind") {
        None | Some("waveform") => awic_proto::RenderKind::Waveform,
        Some("eye") => awic_proto::RenderKind::Eye,
        Some("histogram") => awic_proto::RenderKind::Histogram,
        Some("anomalies") => awic_proto::RenderKind::Anomalies,
        Some(other) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("no view kind {other:?}; use waveform, eye, histogram or anomalies"),
            )
                .into_response()
        }
    };
    let png = match params.get("format") {
        None | Some("svg") => false,
        Some("png") => true,
        Some(other) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("no image format {other:?}; use svg or png"),
            )
                .into_response()
        }
    };
    let interval_kind = match params.get("interval_kind") {
        None => None,
        Some("high") => Some(awic_proto::IntervalKind::High),
        Some("low") => Some(awic_proto::IntervalKind::Low),
        Some("period") => Some(awic_proto::IntervalKind::Period),
        Some(other) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("no interval kind {other:?}; use high, low or period"),
            )
                .into_response()
        }
    };
    let capture_id = match handle.parse::<awic_core::CaptureId>() {
        Ok(id) => id,
        Err(err) => return (StatusCode::BAD_REQUEST, format!("{handle:?}: {err}")).into_response(),
    };
    let Some(stored) = state.capture_store().get(&capture_id) else {
        return (
            StatusCode::NOT_FOUND,
            state
                .capture_store()
                .missing_error(&capture_id, handle)
                .to_string(),
        )
            .into_response();
    };
    let number = |key: &str| -> Option<u32> { params.get(key).and_then(|v| v.parse().ok()) };
    let seconds = |key: &str| -> Option<f64> { params.get(key).and_then(|v| v.parse().ok()) };

    // Both spellings of a channel list, and a channel that is not a number is
    // refused rather than dropped: `channels=4,x` drawing one channel is the
    // same silence as an ignored parameter.
    let mut channels = Vec::new();
    for value in params.all("channels") {
        for part in value.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            match part.parse::<u8>() {
                Ok(channel) => channels.push(channel),
                Err(_) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        format!(
                            "channels={part:?} is not a channel number; give them as \
                             channels=4,6 or channels=4&channels=6"
                        ),
                    )
                        .into_response()
                }
            }
        }
    }

    // `span_s` is the natural zoom, as it is over MCP: t1_s = t0_s + span_s,
    // with t0_s defaulting to 0 (B-36, B-58).
    let t0_s = seconds("t0_s");
    let t1_s = match (seconds("span_s"), seconds("t1_s")) {
        (Some(_), Some(_)) => {
            return (
                StatusCode::BAD_REQUEST,
                "give t1_s or span_s, not both".to_string(),
            )
                .into_response()
        }
        (Some(span), None) if span <= 0.0 => {
            return (
                StatusCode::BAD_REQUEST,
                "span_s must be positive".to_string(),
            )
                .into_response()
        }
        (Some(span), None) => Some(t0_s.unwrap_or(0.0) + span),
        (None, t1) => t1,
    };

    let options = crate::render::Options {
        channels,
        width: number("width"),
        height: number("height"),
        t0_s,
        t1_s,
        period_s: seconds("period_s"),
        interval_kind,
        tolerance: seconds("tolerance"),
        title: params.get("title").map(str::to_string),
    };
    match crate::render::render(&stored, handle, kind, options) {
        Ok(result) if png => match awic_render::png::svg_to_png(&result.svg) {
            Ok(png) => (
                [
                    (header::CONTENT_TYPE, "image/png"),
                    (header::CACHE_CONTROL, "private, max-age=300"),
                ],
                png.bytes,
            )
                .into_response(),
            Err(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response(),
        },
        Ok(result) => (
            [
                (header::CONTENT_TYPE, "image/svg+xml; charset=utf-8"),
                // A capture is immutable once stored, so its view never
                // changes — but the handle is short-lived, so this is a
                // session-length cache, not a public one.
                (header::CACHE_CONTROL, "private, max-age=300"),
            ],
            result.svg,
        )
            .into_response(),
        Err(msg) => (StatusCode::UNPROCESSABLE_ENTITY, msg).into_response(),
    }
}

const PLACEHOLDER_HTML: &str = r#"<!doctype html>
<html>
<head><meta charset="utf-8"><title>AWIC</title></head>
<body style="font-family: sans-serif; max-width: 40rem; margin: 4rem auto; line-height: 1.5;">
<h1>AWIC server is running</h1>
<p>The web frontend has not been built into the configured <code>static_dir</code> yet.</p>
<p>The API is available at <code>/api/health</code> and the WebSocket protocol at <code>/ws</code>.</p>
</body>
</html>
"#;

async fn placeholder_page() -> impl IntoResponse {
    Html(PLACEHOLDER_HTML)
}

/// The frontend compiled into the binary (`embed-web` feature): what a
/// release executable serves when there is no `static_dir` on the host.
#[cfg(feature = "embed-web")]
#[derive(rust_embed::RustEmbed)]
#[folder = "../../web/dist"]
struct EmbeddedWeb;

/// Serves an embedded file, or `index.html` for any path that isn't one (the
/// same SPA fallback `ServeDir` gives `static_dir`).
#[cfg(feature = "embed-web")]
async fn embedded_file(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let (file, path) = match EmbeddedWeb::get(path) {
        Some(file) if !path.is_empty() => (file, path),
        _ => match EmbeddedWeb::get("index.html") {
            Some(file) => (file, "index.html"),
            None => return placeholder_page().await.into_response(),
        },
    };
    // Vite fingerprints everything under assets/, so those never change;
    // index.html must be re-fetched to pick up a new build.
    let cache = if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    (
        [
            (header::CONTENT_TYPE, file.metadata.mimetype().to_string()),
            (header::CACHE_CONTROL, cache.to_string()),
        ],
        file.data,
    )
        .into_response()
}

/// Serves `static_dir` with SPA fallback (any path that isn't a real file
/// under it serves `index.html`); failing that, the frontend embedded in the
/// binary (`embed-web`); failing that, a placeholder page for every path —
/// never a startup failure either way. `static_dir` wins so an operator can
/// swap the frontend without a rebuild.
fn static_router(static_dir: &str) -> Router<AppState> {
    if Path::new(static_dir).is_dir() {
        let index = Path::new(static_dir).join("index.html");
        let serve_dir = ServeDir::new(static_dir).not_found_service(ServeFile::new(index));
        Router::new().fallback_service(serve_dir)
    } else {
        without_static_dir(static_dir)
    }
}

#[cfg(feature = "embed-web")]
fn without_static_dir(static_dir: &str) -> Router<AppState> {
    tracing::info!(
        static_dir,
        "static_dir not found; serving the frontend embedded in the binary"
    );
    Router::new().fallback(embedded_file)
}

#[cfg(not(feature = "embed-web"))]
fn without_static_dir(static_dir: &str) -> Router<AppState> {
    tracing::warn!(
        static_dir,
        "static_dir not found; serving a placeholder page instead of the built frontend"
    );
    Router::new().fallback(placeholder_page)
}

/// `/api/health` plus the static frontend/placeholder fallback.
pub fn router(static_dir: &str) -> Router<AppState> {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/render", get(render_view))
        .merge(static_router(static_dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_html_mentions_the_missing_frontend() {
        assert!(PLACEHOLDER_HTML.contains("static_dir"));
    }
}
