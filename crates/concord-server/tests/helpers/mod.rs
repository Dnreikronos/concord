use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use axum::Router;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::sync::broadcast;
use uuid::Uuid;

use concord_server::routes;
use concord_server::state::AppState;

static POOL: tokio::sync::OnceCell<PgPool> = tokio::sync::OnceCell::const_new();

async fn shared_pool() -> &'static PgPool {
    POOL.get_or_init(|| async {
        let database_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests");

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await
            .expect("failed to connect to test database");

        sqlx::migrate!("../../migrations")
            .run(&pool)
            .await
            .expect("failed to run migrations");

        pool
    })
    .await
}

pub async fn test_app() -> Router {
    let pool = shared_pool().await.clone();
    let (tx, _) = broadcast::channel(256);
    let state = Arc::new(AppState { pool, tx });

    routes::all_routes().with_state(state)
}

pub fn random_username() -> String {
    let id = Uuid::new_v4().simple().to_string();
    format!("u{}", &id[..12])
}

pub fn random_email() -> String {
    format!("{}@test.example.com", Uuid::new_v4().simple())
}

pub fn register_request(body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/auth/register")
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap()
}
