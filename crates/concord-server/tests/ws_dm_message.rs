//! Integration tests for `SendDirectMessage` over WebSocket (issue #23).
//!
//! These exercise the full post-auth DM path — membership validation, message
//! insertion into the shared `messages` table, and the participant-only
//! broadcast — so they need a real Postgres. Set `DATABASE_URL` to a throwaway
//! test database; each test builds its own pool and runs migrations
//! (idempotent). Tests seed their own users and DM channels with random
//! identifiers, so they're safe to run in parallel.

use std::sync::Arc;
use std::time::Duration;

use concord_server::db;
use concord_server::hub::Hub;
use concord_server::jwt;
use concord_server::presence::Presence;
use concord_server::routes;
use concord_server::state::AppState;
use concord_server::typing::{Typing, TYPING_TTL};
use concord_shared::protocol::{ClientMsg, ErrorCode, ServerMsg, Token};

use futures_util::{SinkExt, StreamExt};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

const JWT_SECRET: &str = "test-secret-do-not-use-in-prod";

type ClientWs = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Build a pool owned by the calling test's runtime. See `ws_send_message.rs`
/// for why each test gets its own pool rather than sharing a `static`.
async fn setup_pool() -> PgPool {
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

/// Bind the real router (backed by `pool`) over loopback and return the
/// `ws://.../ws` URL. All clients connecting to the returned URL share one
/// `Hub`, so broadcasts cross connections.
async fn spawn_server(pool: PgPool) -> String {
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

async fn connect(url: &str) -> ClientWs {
    let (ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    ws
}

async fn send(ws: &mut ClientWs, msg: &ClientMsg) {
    let text = serde_json::to_string(msg).unwrap();
    ws.send(Message::Text(text.into())).await.unwrap();
}

/// Read frames until a non-presence `ServerMsg` text frame arrives, bounded by
/// a timeout so a hung test fails fast.
///
/// Presence frames (the connect-time `PresenceSnapshot` and any
/// `UserStatusChanged`) are emitted independently of DM traffic, so they're
/// skipped here — these tests assert on the DM messages they actually sent.
async fn recv(ws: &mut ClientWs) -> ServerMsg {
    let fut = async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(t))) => {
                    match serde_json::from_str::<ServerMsg>(&t).unwrap() {
                        ServerMsg::PresenceSnapshot { .. }
                        | ServerMsg::UserStatusChanged { .. } => continue,
                        other => return other,
                    }
                }
                Some(Ok(_)) => continue,
                other => panic!("expected text frame, got {other:?}"),
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(10), fut)
        .await
        .expect("timed out waiting for server message")
}

/// Open a connection, authenticate as `user_id`, and assert the handshake
/// succeeds. Leaves the socket ready for post-auth messages.
async fn connect_authed(url: &str, user_id: Uuid) -> ClientWs {
    let mut ws = connect(url).await;
    let token = jwt::encode_access_token(user_id, JWT_SECRET).unwrap();
    send(&mut ws, &ClientMsg::Authenticate { token: Token::new(token) }).await;
    match recv(&mut ws).await {
        ServerMsg::Authenticated { user_id: got } => assert_eq!(got, user_id),
        other => panic!("expected Authenticated, got {other:?}"),
    }
    ws
}

fn random_username() -> String {
    let id = Uuid::new_v4().simple().to_string();
    format!("u{}", &id[..12])
}

fn random_email() -> String {
    format!("{}@test.example.com", Uuid::new_v4().simple())
}

/// Insert a fresh user with random credentials.
async fn seed_user(pool: &PgPool) -> Uuid {
    db::insert_user(pool, &random_username(), &random_email(), "test-hash")
        .await
        .expect("insert user")
        .id
}

/// Open a 1:1 DM between two users and return its channel id.
async fn seed_dm(pool: &PgPool, a: Uuid, b: Uuid) -> Uuid {
    db::find_or_create_dm_channel(pool, a, b)
        .await
        .expect("create dm channel")
        .0
        .id
}

#[tokio::test]
async fn dm_message_persists_and_broadcasts_to_author() {
    let pool = setup_pool().await;
    let author = seed_user(&pool).await;
    let other = seed_user(&pool).await;
    let dm = seed_dm(&pool, author, other).await;

    let url = spawn_server(pool.clone()).await;
    let mut ws = connect_authed(&url, author).await;

    let before = chrono::Utc::now();
    send(
        &mut ws,
        &ClientMsg::SendDirectMessage {
            dm_channel_id: dm,
            content: "hello dm".into(),
        },
    )
    .await;

    match recv(&mut ws).await {
        ServerMsg::NewDirectMessage {
            id,
            dm_channel_id,
            author_id,
            content,
            created_at,
        } => {
            assert_eq!(dm_channel_id, dm);
            assert_eq!(author_id, Some(author));
            assert_eq!(content, "hello dm");

            // The broadcast carries the server-stamped time, not a client
            // guess: it lands within a sane window of the send, never the epoch
            // default a missing or mis-wired field would produce.
            assert!(created_at >= before - chrono::Duration::seconds(5));
            assert!(created_at <= chrono::Utc::now() + chrono::Duration::seconds(5));

            // The broadcast id must point at a row actually persisted under the
            // DM channel id — proves DM messages land in the shared table.
            let stored = db::get_message_channel(&pool, id).await.unwrap();
            assert_eq!(stored, Some(dm));
        }
        other => panic!("expected NewDirectMessage, got {other:?}"),
    }
}

#[tokio::test]
async fn dm_message_reaches_other_participant() {
    let pool = setup_pool().await;
    let a = seed_user(&pool).await;
    let b = seed_user(&pool).await;
    let dm = seed_dm(&pool, a, b).await;

    let url = spawn_server(pool.clone()).await;
    // DM fan-out targets members by user id (not a channel subscription), and a
    // connection is registered before its `Authenticated` ack, so both sockets
    // are in the delivery set the moment `connect_authed` returns.
    let mut ws_a = connect_authed(&url, a).await;
    let mut ws_b = connect_authed(&url, b).await;

    send(
        &mut ws_a,
        &ClientMsg::SendDirectMessage {
            dm_channel_id: dm,
            content: "ping".into(),
        },
    )
    .await;

    // Author echo and the recipient both observe the message.
    for ws in [&mut ws_a, &mut ws_b] {
        match recv(ws).await {
            ServerMsg::NewDirectMessage {
                dm_channel_id,
                author_id,
                content,
                ..
            } => {
                assert_eq!(dm_channel_id, dm);
                assert_eq!(author_id, Some(a));
                assert_eq!(content, "ping");
            }
            other => panic!("expected NewDirectMessage, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn dm_message_by_non_member_is_forbidden() {
    let pool = setup_pool().await;
    let a = seed_user(&pool).await;
    let b = seed_user(&pool).await;
    let dm = seed_dm(&pool, a, b).await;

    // A real, registered user who isn't a participant of the DM.
    let outsider = seed_user(&pool).await;
    let url = spawn_server(pool.clone()).await;
    let mut ws = connect_authed(&url, outsider).await;

    send(
        &mut ws,
        &ClientMsg::SendDirectMessage {
            dm_channel_id: dm,
            content: "intruder".into(),
        },
    )
    .await;

    match recv(&mut ws).await {
        ServerMsg::Error { code, message } => {
            assert_eq!(code, ErrorCode::Forbidden);
            assert_eq!(message, "not a member of this DM");
        }
        other => panic!("expected Forbidden error, got {other:?}"),
    }

    // Nothing should have been persisted for the rejected send.
    let count = db::list_channel_messages(&pool, dm, None, 10)
        .await
        .unwrap()
        .len();
    assert_eq!(count, 0, "non-member send must not persist a row");
}

#[tokio::test]
async fn dm_message_to_unknown_channel_is_forbidden() {
    let pool = setup_pool().await;
    let user = seed_user(&pool).await;

    let url = spawn_server(pool.clone()).await;
    let mut ws = connect_authed(&url, user).await;

    // A channel id with no DM behind it has no members, so the membership check
    // rejects it the same way it rejects a real DM the caller isn't in — we
    // never confirm whether the channel exists.
    send(
        &mut ws,
        &ClientMsg::SendDirectMessage {
            dm_channel_id: Uuid::new_v4(),
            content: "into the void".into(),
        },
    )
    .await;

    match recv(&mut ws).await {
        ServerMsg::Error { code, message } => {
            assert_eq!(code, ErrorCode::Forbidden);
            assert_eq!(message, "not a member of this DM");
        }
        other => panic!("expected Forbidden error, got {other:?}"),
    }
}

#[tokio::test]
async fn blank_dm_is_rejected_before_membership_check() {
    let pool = setup_pool().await;
    let user = seed_user(&pool).await;

    // No DM is seeded for `user`, so a passing membership check would surface
    // Forbidden. Getting BadRequest instead proves validation runs first.
    let url = spawn_server(pool.clone()).await;
    let mut ws = connect_authed(&url, user).await;

    send(
        &mut ws,
        &ClientMsg::SendDirectMessage {
            dm_channel_id: Uuid::new_v4(),
            content: "   ".into(),
        },
    )
    .await;

    match recv(&mut ws).await {
        ServerMsg::Error { code, message } => {
            assert_eq!(code, ErrorCode::BadRequest);
            assert_eq!(message, "message content must not be blank");
        }
        other => panic!("expected BadRequest error, got {other:?}"),
    }
}

#[tokio::test]
async fn oversized_dm_is_rejected() {
    let pool = setup_pool().await;
    let a = seed_user(&pool).await;
    let b = seed_user(&pool).await;
    let dm = seed_dm(&pool, a, b).await;

    let url = spawn_server(pool.clone()).await;
    let mut ws = connect_authed(&url, a).await;

    // One past the 4000-char cap; the DM path must surface the same rejection
    // the validator unit-tests cover.
    let too_long = "a".repeat(4001);
    send(
        &mut ws,
        &ClientMsg::SendDirectMessage {
            dm_channel_id: dm,
            content: too_long,
        },
    )
    .await;

    match recv(&mut ws).await {
        ServerMsg::Error { code, message } => {
            assert_eq!(code, ErrorCode::BadRequest);
            assert_eq!(message, "message content must be at most 4000 characters");
        }
        other => panic!("expected BadRequest error, got {other:?}"),
    }
}
