// Shared across integration-test crates; not every test uses every helper.
#![allow(dead_code)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use concord_server::hub::Hub;
use concord_server::presence::Presence;
use concord_server::routes;
use concord_server::state::AppState;
use concord_server::typing::{Typing, TYPING_TTL};

/// JWT signing secret wired into `test_app`; reuse it to mint tokens the app
/// will accept.
pub const JWT_SECRET: &str = "test-secret-do-not-use-in-prod";

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

/// Build the full router backed by `pool`.
pub fn app_with_pool(pool: PgPool) -> Router {
    let hub = Arc::new(Hub::new());
    let typing = Arc::new(Typing::new(Arc::clone(&hub), TYPING_TTL, None));
    let state = Arc::new(AppState {
        pool,
        hub,
        typing,
        presence: Presence::disabled(),
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

/// A `Bearer` header value for an access token signed for `user_id`.
pub fn auth_header(user_id: Uuid) -> String {
    let token = concord_server::jwt::encode_access_token(user_id, JWT_SECRET).unwrap();
    format!("Bearer {token}")
}

/// A `GET uri` request authenticated as `user_id`.
pub fn authed_get(uri: &str, user_id: Uuid) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", auth_header(user_id))
        .body(Body::empty())
        .unwrap()
}

/// A `POST uri` request with a JSON `body`, authenticated as `user_id`.
pub fn authed_post(uri: &str, user_id: Uuid, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("authorization", auth_header(user_id))
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

/// Insert a password-auth user (optionally with an avatar). Returns its id and
/// generated username.
pub async fn seed_user(pool: &PgPool, avatar_url: Option<&str>) -> (Uuid, String) {
    let username = random_username();
    let id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO users (username, password_hash, avatar_url) \
         VALUES ($1, 'x', $2) RETURNING id",
    )
    .bind(&username)
    .bind(avatar_url)
    .fetch_one(pool)
    .await
    .unwrap();
    (id, username)
}

/// Create a server owned by `owner_id` (added as an admin member) plus one text
/// channel. Returns `(server_id, channel_id)`.
pub async fn seed_server_channel(pool: &PgPool, owner_id: Uuid) -> (Uuid, Uuid) {
    let server_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO servers (name, owner_id) VALUES ('test-server', $1) RETURNING id",
    )
    .bind(owner_id)
    .fetch_one(pool)
    .await
    .unwrap();

    seed_server_member(pool, server_id, owner_id, "admin").await;

    let channel_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO channels (server_id, name, channel_type) \
         VALUES ($1, 'general', 'text') RETURNING id",
    )
    .bind(server_id)
    .fetch_one(pool)
    .await
    .unwrap();

    (server_id, channel_id)
}

pub async fn seed_server_member(pool: &PgPool, server_id: Uuid, user_id: Uuid, role: &str) {
    sqlx::query("INSERT INTO server_members (server_id, user_id, role) VALUES ($1, $2, $3)")
        .bind(server_id)
        .bind(user_id)
        .bind(role)
        .execute(pool)
        .await
        .unwrap();
}

/// Create a 1:1 DM channel with the given members. Returns the dm channel id.
pub async fn seed_dm_channel(pool: &PgPool, members: &[Uuid]) -> Uuid {
    let dm_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO dm_channels (is_group) VALUES (false) RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();

    for &user_id in members {
        sqlx::query("INSERT INTO dm_members (dm_channel_id, user_id) VALUES ($1, $2)")
            .bind(dm_id)
            .bind(user_id)
            .execute(pool)
            .await
            .unwrap();
    }

    dm_id
}

/// Insert a message with an explicit `created_at` so ordering and cursor tests
/// are deterministic. `author_id` may be `None` to simulate a deleted author.
pub async fn seed_message_at(
    pool: &PgPool,
    channel_id: Uuid,
    author_id: Option<Uuid>,
    content: &str,
    created_at: DateTime<Utc>,
) -> Uuid {
    sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO messages (channel_id, author_id, content, created_at) \
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(channel_id)
    .bind(author_id)
    .bind(content)
    .bind(created_at)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Bulk-insert `count` messages (`"msg 0"..="msg {count-1}"`) one second apart
/// starting at `base`, so `"msg {count-1}"` is the newest. One round trip.
pub async fn seed_messages_bulk(
    pool: &PgPool,
    channel_id: Uuid,
    author_id: Option<Uuid>,
    count: i64,
    base: DateTime<Utc>,
) {
    sqlx::query(
        "INSERT INTO messages (channel_id, author_id, content, created_at) \
         SELECT $1, $2, 'msg ' || g, $3 + make_interval(secs => g) \
         FROM generate_series(0, $4 - 1) AS g",
    )
    .bind(channel_id)
    .bind(author_id)
    .bind(base)
    .bind(count)
    .execute(pool)
    .await
    .unwrap();
}
