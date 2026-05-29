//! Integration tests for the user-presence system (issue #20).
//!
//! These drive the real `ws_handler` over loopback TCP against a live Postgres
//! (so the shared-server peer query runs for real) and seed users/servers
//! directly via SQL. The presence *broadcast* path is pure in-process hub
//! routing, so those tests run with persistence disabled. The snapshot test
//! reads back from Redis and is skipped unless `REDIS_URL` is set.
//!
//! Requires `DATABASE_URL` to point at a migratable Postgres, matching the
//! existing `register` integration tests.

use std::sync::Arc;
use std::time::Duration;

use concord_server::hub::Hub;
use concord_server::jwt;
use concord_server::presence::Presence;
use concord_server::routes;
use concord_server::state::AppState;
use concord_server::typing::{Typing, TYPING_TTL};
use concord_shared::protocol::{ClientMsg, ServerMsg, Token};
use concord_shared::types::UserStatus;

use futures_util::{SinkExt, StreamExt};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

const JWT_SECRET: &str = "test-secret-do-not-use-in-prod";

type ClientWs = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// A fresh, migrated pool. Built per test (not shared in a static) so each
/// test's connections belong to its own runtime and can't strand across
/// runtimes.
async fn test_pool() -> PgPool {
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

/// A presence store backed by `REDIS_URL` when set, otherwise disabled. The
/// broadcast tests don't care which they get; the snapshot test requires the
/// enabled variant and skips otherwise.
async fn test_presence() -> Presence {
    match std::env::var("REDIS_URL").ok().filter(|s| !s.is_empty()) {
        Some(url) => Presence::connect(&url, Duration::from_secs(60))
            .await
            .expect("failed to connect to test redis"),
        None => Presence::disabled(),
    }
}

/// Bind the real router over loopback with the given pool + presence store and
/// return the `ws://.../ws` URL.
async fn spawn_server(pool: PgPool, presence: Presence) -> String {
    let hub = Arc::new(Hub::new());
    let typing = Arc::new(Typing::new(Arc::clone(&hub), TYPING_TTL, None));
    let state = Arc::new(AppState {
        pool,
        hub,
        typing,
        presence,
        jwt_secret: secrecy::SecretString::from(JWT_SECRET),
        github_oauth: None,
        google_oauth: None,
        http_client: reqwest::Client::new(),
        ws_auth_timeout: Duration::from_secs(10),
    });

    let app = routes::all_routes().with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    format!("ws://{addr}/ws")
}

async fn insert_user(pool: &PgPool) -> Uuid {
    let username = format!("u{}", &Uuid::new_v4().simple().to_string()[..12]);
    let email = format!("{}@test.example.com", Uuid::new_v4().simple());
    sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO users (username, email, password_hash) \
         VALUES ($1, $2, 'x') RETURNING id",
    )
    .bind(&username)
    .bind(&email)
    .fetch_one(pool)
    .await
    .expect("insert user")
}

async fn insert_server(pool: &PgPool, owner_id: Uuid) -> Uuid {
    sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO servers (name, owner_id) VALUES ('test-server', $1) RETURNING id",
    )
    .bind(owner_id)
    .fetch_one(pool)
    .await
    .expect("insert server")
}

async fn add_member(pool: &PgPool, server_id: Uuid, user_id: Uuid) {
    sqlx::query("INSERT INTO server_members (server_id, user_id, role) VALUES ($1, $2, 'member')")
        .bind(server_id)
        .bind(user_id)
        .execute(pool)
        .await
        .expect("add member");
}

/// Two users sharing one server.
async fn seed_shared_server(pool: &PgPool) -> (Uuid, Uuid) {
    let a = insert_user(pool).await;
    let b = insert_user(pool).await;
    let server = insert_server(pool, a).await;
    add_member(pool, server, a).await;
    add_member(pool, server, b).await;
    (a, b)
}

async fn connect(url: &str) -> ClientWs {
    let (ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    ws
}

async fn send(ws: &mut ClientWs, msg: &ClientMsg) {
    let text = serde_json::to_string(msg).unwrap();
    ws.send(Message::Text(text.into())).await.unwrap();
}

/// Read the next `ServerMsg` text frame, skipping control frames, bounded by a
/// timeout so a hung test fails fast.
async fn recv(ws: &mut ClientWs) -> ServerMsg {
    let fut = async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(t))) => {
                    return serde_json::from_str::<ServerMsg>(&t).unwrap()
                }
                Some(Ok(_)) => continue,
                other => panic!("expected text frame, got {other:?}"),
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(3), fut)
        .await
        .expect("timed out waiting for server message")
}

/// Assert no `ServerMsg` text frame arrives within `window`.
async fn expect_silence(ws: &mut ClientWs, window: Duration) {
    let fut = async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(t))) => {
                    let msg = serde_json::from_str::<ServerMsg>(&t).unwrap();
                    panic!("expected silence, got {msg:?}");
                }
                Some(Ok(_)) => continue,
                Some(Err(err)) => {
                    panic!("connection errored while waiting for silence: {err:?}")
                }
                None => panic!("connection closed while waiting for silence"),
            }
        }
    };
    // A timeout is the only success case: a dropped socket must not pass as quiet.
    assert!(
        tokio::time::timeout(window, fut).await.is_err(),
        "expected no server message for {window:?}",
    );
}

/// Authenticate and consume the `Authenticated` + initial `PresenceSnapshot`
/// frames, returning the snapshot's peer list.
async fn authenticate(ws: &mut ClientWs, uid: Uuid) -> Vec<concord_shared::protocol::UserPresence> {
    let token = jwt::encode_access_token(uid, JWT_SECRET).unwrap();
    send(ws, &ClientMsg::Authenticate { token: Token::new(token) }).await;

    match recv(ws).await {
        ServerMsg::Authenticated { user_id } => assert_eq!(user_id, uid),
        other => panic!("expected Authenticated, got {other:?}"),
    }
    match recv(ws).await {
        ServerMsg::PresenceSnapshot { users } => users,
        other => panic!("expected PresenceSnapshot, got {other:?}"),
    }
}

#[tokio::test]
async fn peer_is_notified_when_user_comes_online() {
    let pool = test_pool().await;
    let (a, b) = seed_shared_server(&pool).await;
    let url = spawn_server(pool, Presence::disabled()).await;

    let mut ws_a = connect(&url).await;
    authenticate(&mut ws_a, a).await;

    // B comes online — A should be told.
    let mut ws_b = connect(&url).await;
    authenticate(&mut ws_b, b).await;

    match recv(&mut ws_a).await {
        ServerMsg::UserStatusChanged { user_id, status } => {
            assert_eq!(user_id, b);
            assert_eq!(status, UserStatus::Online);
        }
        other => panic!("expected UserStatusChanged online, got {other:?}"),
    }
}

#[tokio::test]
async fn peer_is_notified_of_manual_status_update() {
    let pool = test_pool().await;
    let (a, b) = seed_shared_server(&pool).await;
    let url = spawn_server(pool, Presence::disabled()).await;

    let mut ws_a = connect(&url).await;
    authenticate(&mut ws_a, a).await;

    let mut ws_b = connect(&url).await;
    authenticate(&mut ws_b, b).await;

    // Drain A's notification of B coming online.
    let _ = recv(&mut ws_a).await;

    send(&mut ws_b, &ClientMsg::UpdateStatus { status: UserStatus::Dnd }).await;

    match recv(&mut ws_a).await {
        ServerMsg::UserStatusChanged { user_id, status } => {
            assert_eq!(user_id, b);
            assert_eq!(status, UserStatus::Dnd);
        }
        other => panic!("expected UserStatusChanged dnd, got {other:?}"),
    }
}

#[tokio::test]
async fn peer_is_notified_when_user_goes_offline() {
    let pool = test_pool().await;
    let (a, b) = seed_shared_server(&pool).await;
    let url = spawn_server(pool, Presence::disabled()).await;

    let mut ws_a = connect(&url).await;
    authenticate(&mut ws_a, a).await;

    let mut ws_b = connect(&url).await;
    authenticate(&mut ws_b, b).await;

    // Drain A's notification of B coming online.
    let _ = recv(&mut ws_a).await;

    // B disconnects cleanly.
    ws_b.close(None).await.unwrap();

    match recv(&mut ws_a).await {
        ServerMsg::UserStatusChanged { user_id, status } => {
            assert_eq!(user_id, b);
            assert_eq!(status, UserStatus::Offline);
        }
        other => panic!("expected UserStatusChanged offline, got {other:?}"),
    }
}

#[tokio::test]
async fn presence_is_not_shared_across_unrelated_servers() {
    let pool = test_pool().await;
    // A and C are each alone in their own server — no shared membership.
    let a = insert_user(&pool).await;
    let c = insert_user(&pool).await;
    let server_a = insert_server(&pool, a).await;
    add_member(&pool, server_a, a).await;
    let server_c = insert_server(&pool, c).await;
    add_member(&pool, server_c, c).await;

    let url = spawn_server(pool, Presence::disabled()).await;

    let mut ws_a = connect(&url).await;
    let snapshot = authenticate(&mut ws_a, a).await;
    assert!(snapshot.is_empty(), "A should see no peers, got {snapshot:?}");

    // C connects; A must hear nothing about it.
    let mut ws_c = connect(&url).await;
    authenticate(&mut ws_c, c).await;

    expect_silence(&mut ws_a, Duration::from_millis(500)).await;
}

#[tokio::test]
async fn snapshot_lists_online_peers() {
    let presence = test_presence().await;
    if !presence.is_enabled() {
        eprintln!("skipping snapshot_lists_online_peers: REDIS_URL not set");
        return;
    }

    let pool = test_pool().await;
    let (a, b) = seed_shared_server(&pool).await;
    let url = spawn_server(pool, presence).await;

    // A connects first and is persisted as online in Redis.
    let mut ws_a = connect(&url).await;
    let a_snapshot = authenticate(&mut ws_a, a).await;
    assert!(a_snapshot.is_empty(), "B is still offline; A's snapshot should be empty");

    // B connects and should see A in its initial snapshot.
    let mut ws_b = connect(&url).await;
    let b_snapshot = authenticate(&mut ws_b, b).await;

    assert_eq!(b_snapshot.len(), 1, "expected exactly A in snapshot, got {b_snapshot:?}");
    assert_eq!(b_snapshot[0].user_id, a);
    assert_eq!(b_snapshot[0].status, UserStatus::Online);
}
