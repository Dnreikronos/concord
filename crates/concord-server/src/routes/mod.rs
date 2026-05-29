pub mod auth;
pub mod categories;
pub mod channels;
pub mod dms;
pub mod oauth;
pub mod servers;
pub mod ws;

use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::routing::get;
use axum::Router;

use crate::state::AppState;

pub fn all_routes() -> Router<Arc<AppState>> {
    Router::new()
        .nest("/api/auth", auth::router())
        .nest("/api/auth/oauth", oauth::router())
        .nest("/api/servers", servers::router())
        .nest("/api/channels", channels::router())
        .nest("/api/categories", categories::router())
        .nest("/api/dms", dms::router())
        .route("/ws", get(ws::ws_handler))
        .layer(DefaultBodyLimit::max(1024 * 1024))
}
