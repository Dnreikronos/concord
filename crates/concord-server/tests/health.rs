mod helpers;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use helpers::app_with_pool;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

/// `/health` must answer without touching the database, so a lazy pool that
/// never opens a connection is enough to prove the route is wired and returns
/// `200 OK`. This keeps the liveness probe testable without the integration DB.
#[tokio::test]
async fn health_returns_ok_without_db() {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
        .expect("lazy pool should build from a well-formed url");
    let app = app_with_pool(pool);

    let resp = app
        .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
}
