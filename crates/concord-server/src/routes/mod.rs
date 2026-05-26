pub mod auth;

use axum::extract::DefaultBodyLimit;
use axum::Router;
use sqlx::PgPool;

pub fn all_routes() -> Router<PgPool> {
    Router::new()
        .nest("/api/auth", auth::router())
        .layer(DefaultBodyLimit::max(1024 * 1024))
}
