pub mod auth;

use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::Router;

use crate::state::AppState;

pub fn all_routes() -> Router<Arc<AppState>> {
    Router::new()
        .nest("/api/auth", auth::router())
        .layer(DefaultBodyLimit::max(1024 * 1024))
}
