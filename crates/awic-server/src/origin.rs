//! Cross-origin guard for `/ws` and `/mcp` (SPECIFICATION.md §12).
//!
//! Browsers enforce no same-origin policy on WebSocket upgrades, and a plain
//! `text/plain` POST needs no CORS preflight. With no token configured — the
//! default — any page the user happens to visit could otherwise open
//! `ws://127.0.0.1:8080/ws` and switch USB power or write to a UART: a
//! physical effect triggered by a website. The MCP transport spec asks
//! servers to validate `Origin` for the same reason (DNS rebinding).
//!
//! The rule: a request **without** an `Origin` header comes from a non-browser
//! client (a script, an MCP client, `awic mcp-stdio`) and passes — this guard
//! is not authentication, the optional token is. A request **with** one must
//! come from the server's own origin (its `Host`), or from a configured
//! `[server] allowed_origins` entry.

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::state::AppState;

pub async fn require_same_origin(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .map(|v| v.to_str().unwrap_or("<non-ascii>").to_string());
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok());
    match origin {
        None => next.run(req).await,
        Some(origin) if origin_allowed(&origin, host, &state.config().server.allowed_origins) => {
            next.run(req).await
        }
        Some(origin) => {
            tracing::warn!(%origin, host = host.unwrap_or(""), path = %req.uri().path(),
                "refused a cross-origin request");
            (
                StatusCode::FORBIDDEN,
                format!(
                    "cross-origin request refused: Origin {origin} is not this server ({}); \
                     add it to [server] allowed_origins if it should be trusted",
                    host.unwrap_or("unknown host")
                ),
            )
                .into_response()
        }
    }
}

/// Same-origin means the Origin's authority equals the request's `Host` —
/// true for the UI served by this server, however the user reached it
/// (localhost, a LAN IP, a hostname). `null` (sandboxed frames, `file://`)
/// never matches.
fn origin_allowed(origin: &str, host: Option<&str>, allowed: &[String]) -> bool {
    if allowed.iter().any(|a| a.eq_ignore_ascii_case(origin)) {
        return true;
    }
    let authority = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"));
    match (authority, host) {
        (Some(authority), Some(host)) => authority.eq_ignore_ascii_case(host),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::origin_allowed;

    fn dev() -> Vec<String> {
        vec!["http://localhost:5173".to_string()]
    }

    #[test]
    fn the_ui_served_by_this_server_is_same_origin() {
        assert!(origin_allowed(
            "http://127.0.0.1:8080",
            Some("127.0.0.1:8080"),
            &[]
        ));
        assert!(origin_allowed(
            "http://192.168.1.20:8080",
            Some("192.168.1.20:8080"),
            &[]
        ));
        assert!(origin_allowed("https://bench.lan", Some("BENCH.lan"), &[]));
    }

    #[test]
    fn another_site_is_refused() {
        assert!(!origin_allowed(
            "https://evil.example",
            Some("127.0.0.1:8080"),
            &[]
        ));
        // Same host, different port: a different origin.
        assert!(!origin_allowed(
            "http://127.0.0.1:9999",
            Some("127.0.0.1:8080"),
            &[]
        ));
    }

    #[test]
    fn null_and_unparsable_origins_are_refused() {
        assert!(!origin_allowed("null", Some("127.0.0.1:8080"), &[]));
        assert!(!origin_allowed(
            "file:///tmp/x.html",
            Some("127.0.0.1:8080"),
            &[]
        ));
        assert!(!origin_allowed("http://127.0.0.1:8080", None, &[]));
    }

    #[test]
    fn configured_origins_pass_even_when_host_differs() {
        // The Vite dev proxy rewrites Host to the backend but forwards Origin.
        assert!(origin_allowed(
            "http://localhost:5173",
            Some("127.0.0.1:8080"),
            &dev()
        ));
        assert!(!origin_allowed(
            "http://localhost:5174",
            Some("127.0.0.1:8080"),
            &dev()
        ));
    }
}
