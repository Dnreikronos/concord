// Shared across integration-test crates; not every test uses every helper.
#![allow(dead_code)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use axum::Router;
use chrono::{DateTime, Utc};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use uuid::Uuid;

use concord_server::hub::Hub;
use concord_server::presence::Presence;
use concord_server::routes;
use concord_server::state::AppState;
use concord_server::typing::{Typing, TYPING_TTL};

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
