use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use axum::Router;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use uuid::Uuid;

use concord_server::hub::Hub;
use concord_server::routes;
use concord_server::state::AppState;

/// JWT signing secret wired into `test_app`; reuse it to mint tokens the app
/// will accept.
pub const JWT_SECRET: &str = "test-secret-do-not-use-in-prod";

pub async fn test_app() -> Router {
    app_with_pool(setup_pool().await)
}

/// Build a fresh pool bound to the calling test's runtime, migrations applied.
///
/// Each `#[tokio::test]` runs its own current-thread runtime and sqlx binds a
/// connection to whichever runtime opened it. A pool shared across tests (e.g.
/// via a `static OnceCell`) therefore strands connections from already-dropped
/// runtimes, and later tests starve on acquire — so every test gets its own
/// pool instead. Migrations are idempotent and advisory-lock-guarded, so
/// re-running per test is cheap and safe.
pub async fn setup_pool() -> PgPool {
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
}

/// Build the full router backed by `pool`.
pub fn app_with_pool(pool: PgPool) -> Router {
    let state = Arc::new(AppState {
        pool,
        hub: Arc::new(Hub::new()),
        jwt_secret: secrecy::SecretString::from(JWT_SECRET),
        github_oauth: None,
        google_oauth: None,
        http_client: reqwest::Client::new(),
        ws_auth_timeout: std::time::Duration::from_secs(10),
    });

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
