pub mod auth;
pub mod categories;
pub mod channels;
pub mod dms;
pub mod oauth;
pub mod servers;
pub mod users;
pub mod ws;

use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::routing::get;
use axum::Router;

use crate::state::AppState;

pub fn all_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/health", get(health))
        .nest("/api/auth", auth::router())
        .nest("/api/auth/oauth", oauth::router())
        .nest("/api/servers", servers::router())
        .nest("/api/channels", channels::router())
        .nest("/api/categories", categories::router())
        .nest("/api/dms", dms::router())
        .nest("/api/users", users::router())
        .route("/ws", get(ws::ws_handler))
        .layer(DefaultBodyLimit::max(1024 * 1024))
}

/// Liveness probe used by the Docker `HEALTHCHECK` and any orchestrator. It does
/// no work (no DB or Redis round-trip) and simply answers `200 OK` to confirm the
/// process is up and serving requests.
async fn health() -> StatusCode {
    StatusCode::OK
}
