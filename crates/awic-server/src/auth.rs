//! The optional bearer token (SPECIFICATION.md §12, R-01): if `[auth].token`
//! is set in config, every HTTP route *and* the `/ws` upgrade require it.
//! Unset (the default) means no auth at all, which is the specified
//! out-of-the-box behavior.

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::state::AppState;

/// Applied as a layer over the whole router (SPEC §12): the `/ws` upgrade is
/// an ordinary HTTP request until the moment it's upgraded, so this runs for
/// it exactly like any other route.
pub async fn require_auth(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    match &state.config().auth.token {
        None => next.run(req).await,
        Some(expected) => {
            if is_authorized(&req, expected) {
                next.run(req).await
            } else {
                (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response()
            }
        }
    }
}

/// A request is authorized if it carries `Authorization: Bearer <token>`, or
/// — since browser `WebSocket` clients cannot set arbitrary headers on the
/// upgrade request — a `?token=<token>` query parameter with the same value.
fn is_authorized(req: &Request<Body>, expected: &str) -> bool {
    let header_token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if header_token.is_some_and(|t| constant_time_eq(t.as_bytes(), expected.as_bytes())) {
        return true;
    }

    // Percent-decode the query value: a browser URL-encodes `+`, `/`, `=`
    // and `%`, so a base64-style token would otherwise authenticate over the
    // Authorization header yet fail on the WebSocket path — confusingly, and
    // only there.
    req.uri().query().is_some_and(|query| {
        form_urlencoded::parse(query.as_bytes())
            .any(|(k, v)| k == "token" && constant_time_eq(v.as_bytes(), expected.as_bytes()))
    })
}

/// Compares in time independent of *where* the inputs differ.
///
/// A plain `==` returns at the first mismatching byte, so response latency
/// leaks how long a correct prefix an attacker has guessed. This token is the
/// only thing standing between the LAN and an endpoint that can cut USB power,
/// so it gets the standard fix. The length check does leak the token length,
/// which is conventional and acceptable (the `subtle` crate does the same).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(auth_header: Option<&str>, query: Option<&str>) -> Request<Body> {
        let uri = match query {
            Some(q) => format!("/ws?{q}"),
            None => "/ws".to_string(),
        };
        let mut builder = Request::builder().uri(uri);
        if let Some(value) = auth_header {
            builder = builder.header(header::AUTHORIZATION, value);
        }
        builder.body(Body::empty()).unwrap()
    }

    #[test]
    fn percent_encoded_query_token_is_decoded_before_comparison() {
        // A base64-style token as a browser would send it on the WS upgrade.
        let req = request(None, Some("token=ab%2Bc%2Fd%3D"));
        assert!(is_authorized(&req, "ab+c/d="));
    }

    #[test]
    fn constant_time_eq_matches_plain_equality() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secre"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn bearer_header_with_matching_token_is_authorized() {
        let req = request(Some("Bearer secret"), None);
        assert!(is_authorized(&req, "secret"));
    }

    #[test]
    fn bearer_header_with_wrong_token_is_rejected() {
        let req = request(Some("Bearer wrong"), None);
        assert!(!is_authorized(&req, "secret"));
    }

    #[test]
    fn query_token_with_matching_value_is_authorized() {
        let req = request(None, Some("token=secret"));
        assert!(is_authorized(&req, "secret"));
    }

    #[test]
    fn no_credentials_at_all_is_rejected() {
        let req = request(None, None);
        assert!(!is_authorized(&req, "secret"));
    }
}
