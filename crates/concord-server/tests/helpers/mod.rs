// Shared across several test binaries; not every binary uses every helper.
#![allow(dead_code)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use concord_server::hub::Hub;
use concord_server::routes;
use concord_server::state::AppState;

const JWT_SECRET: &str = "test-secret-do-not-use-in-prod";

/// Build a fresh pool bound to the current test's runtime and run migrations.
///
/// Each `#[tokio::test]` spins up its own current-thread runtime, and sqlx pins
/// every connection to whichever runtime opened it. Sharing one pool across
/// tests via a `static` strands those connections when an early runtime is
/// dropped, and later tests then starve on the acquire timeout — even run
/// serially. Migrations are idempotent and advisory-lock guarded, so opening a
/// pool per test is cheap and safe. `max_connections` is kept small because a
/// single test only ever uses connections sequentially; this bounds the total
/// against Postgres' limit when many test runtimes are live at once.
pub async fn setup_pool() -> PgPool {
    let database_url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests");

    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(&database_url)
        .await
        .expect("failed to connect to test database");

    sqlx::migrate!("../../migrations")
        .run(&pool)
        .await
        .expect("failed to run migrations");

    pool
}

/// Wire the real router onto a caller-provided pool. Tests that need to inspect
/// the database directly can keep a clone of the pool they pass in.
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

pub async fn test_app() -> Router {
    app_with_pool(setup_pool().await)
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

pub struct TestUser {
    pub id: Uuid,
    pub token: String,
}

/// Register a fresh random user and log in, returning the new id and a bearer
/// access token ready for `Authorization` headers.
pub async fn create_user(app: &Router) -> TestUser {
    let email = random_email();
    let password = "securepass1";
    let body = json!({ "username": random_username(), "email": email, "password": password });

    let resp = app
        .clone()
        .oneshot(register_request(&body.to_string()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED, "create_user: register failed");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let user: Value = serde_json::from_slice(&bytes).unwrap();
    let id: Uuid = user["id"].as_str().unwrap().parse().unwrap();

    let login_body = json!({ "email": email, "password": password });
    let login_req = Request::builder()
        .method("POST")
        .uri("/api/auth/login")
        .header("content-type", "application/json")
        .body(Body::from(login_body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(login_req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "create_user: login failed");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let login: Value = serde_json::from_slice(&bytes).unwrap();
    let token = login["access_token"].as_str().unwrap().to_owned();

    TestUser { id, token }
}

/// Build an authenticated request carrying a JSON body.
pub fn auth_json_request(method: &str, uri: &str, token: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_owned()))
        .unwrap()
}

/// Build an authenticated request with no body (e.g. DELETE).
pub fn auth_request(method: &str, uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

/// Send a request through the app and decode the status + JSON body.
pub async fn send_json(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}
