//! Wires the routes, auth layer, and shared state together into the
//! `axum::Router` that `main.rs` (and every integration test) serves.

use axum::middleware;
use axum::routing::get;
use axum::Router;

use crate::auth::require_auth;
use crate::http;
use crate::mcp::route::{handle_delete, handle_post};
use crate::origin::require_same_origin;
use crate::state::AppState;
use crate::ws::ws_handler;

/// Builds the full router: `/api/health`, `/ws`, `/mcp` (SPECIFICATION.md
/// §8.1), and the static frontend (§1.1, §3, §11), with the optional
/// bearer-token layer (§12) wrapping all of it — `/mcp` requires the token
/// exactly like the web UI, with no separate opt-out.
pub fn build_router(state: AppState) -> Router {
    let static_dir = state.config().server.static_dir.clone();

    // The two endpoints that act on the bench refuse foreign browser
    // origins (SPEC §12): a WebSocket has no same-origin protection of its own.
    let control = Router::new()
        .route("/ws", get(ws_handler))
        .route(
            "/mcp",
            axum::routing::post(handle_post).delete(handle_delete),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_same_origin,
        ));

    Router::new()
        .merge(control)
        .merge(http::router(&static_dir))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth))
        .with_state(state)
}
