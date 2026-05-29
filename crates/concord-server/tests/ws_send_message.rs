//! Integration tests for `SendMessage` over WebSocket (issue #16).
//!
//! Unlike `ws_auth.rs`, these exercise the full post-auth path — channel-access
//! checks, message insertion, and the channel broadcast — so they need a real
//! Postgres. Set `DATABASE_URL` to a throwaway test database; each test builds
//! its own pool and runs migrations (idempotent). Tests seed their own
//! server/channel with random identifiers, so they're safe to run in parallel.

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

/// Build a pool owned by the calling test's runtime.
///
/// Each `#[tokio::test]` spins up its own current-thread runtime, and sqlx
/// binds a connection to whichever runtime opened it. A `static` pool shared
/// across tests therefore strands connections the moment an early test's
/// runtime is dropped, starving later tests into the acquire timeout. Giving
/// every test a fresh pool keeps each pool's lifetime inside one runtime.
///
/// Migrations are idempotent and sqlx guards them with an advisory lock, so
/// re-running per test is cheap and safe even when tests run in parallel.
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

/// Read frames until a `ServerMsg` text frame arrives (skipping control frames),
/// bounded by a timeout so a hung test fails fast. The window is generous
/// because these tests run in parallel against a real DB: cold-start
/// congestion (opening pools, running migrations, several tests firing at
/// once) can briefly delay a round-trip past a tighter bound.
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

/// Insert a server owned by `owner_id`, register `owner_id` as a member, and
/// add one text channel. Returns `(server_id, channel_id)`.
async fn seed_server_with_channel(pool: &PgPool, owner_id: Uuid) -> (Uuid, Uuid) {
    let server = db::insert_server(pool, "test-server", None, owner_id)
        .await
        .expect("insert server");
    db::insert_server_member(pool, server.id, owner_id, "member")
        .await
        .expect("insert server member");
    let channel = db::insert_channel(pool, server.id, "general", None, "text")
        .await
        .expect("insert channel");
    (server.id, channel.id)
}

#[tokio::test]
async fn send_message_persists_and_broadcasts_to_author() {
    let pool = setup_pool().await;
    let author = seed_user(&pool).await;
    let (_server, channel) = seed_server_with_channel(&pool, author).await;

    let url = spawn_server(pool.clone()).await;
    let mut ws = connect_authed(&url, author).await;

    let before = chrono::Utc::now();
    send(
        &mut ws,
        &ClientMsg::SendMessage {
            channel_id: channel,
            content: "hello world".into(),
        },
    )
    .await;

    match recv(&mut ws).await {
        ServerMsg::NewMessage {
            id,
            channel_id,
            author_id,
            content,
            created_at,
        } => {
            assert_eq!(channel_id, channel);
            assert_eq!(author_id, Some(author));
            assert_eq!(content, "hello world");

            // The broadcast carries the server-stamped time, not a client
            // guess: it lands within a sane window of the send, never the epoch
            // default a missing or mis-wired field would produce.
            assert!(created_at >= before - chrono::Duration::seconds(5));
            assert!(created_at <= chrono::Utc::now() + chrono::Duration::seconds(5));

            // The broadcast id must point at a row actually persisted in this
            // channel — proves the insert happened, not just the fan-out.
            let stored = db::get_message_channel(&pool, id).await.unwrap();
            assert_eq!(stored, Some(channel));
        }
        other => panic!("expected NewMessage, got {other:?}"),
    }
}

#[tokio::test]
async fn send_message_reaches_other_channel_subscriber() {
    let pool = setup_pool().await;
    let author = seed_user(&pool).await;
    let other = seed_user(&pool).await;
    let (server, channel) = seed_server_with_channel(&pool, author).await;
    // `other` joins the same server, so the auth-time subscription wires them
    // into the channel's broadcast set.
    db::insert_server_member(&pool, server, other, "member")
        .await
        .expect("insert second member");

    let url = spawn_server(pool.clone()).await;

    // The server subscribes a connection only after it sends `Authenticated`
    // (ws.rs), so returning from the handshake doesn't prove a client is in the
    // broadcast set yet. Connect `other` first and round-trip its own message:
    // receiving that echo proves `other` is subscribed, since the server
    // subscribes before reading any post-auth frame. Connect the author only
    // afterward, so the warm-up never lands on the author's socket.
    let mut other_ws = connect_authed(&url, other).await;
    send(
        &mut other_ws,
        &ClientMsg::SendMessage {
            channel_id: channel,
            content: "warmup".into(),
        },
    )
    .await;
    match recv(&mut other_ws).await {
        ServerMsg::NewMessage { content, .. } => assert_eq!(content, "warmup"),
        other => panic!("expected warmup NewMessage, got {other:?}"),
    }

    let mut author_ws = connect_authed(&url, author).await;
    send(
        &mut author_ws,
        &ClientMsg::SendMessage {
            channel_id: channel,
            content: "ping".into(),
        },
    )
    .await;

    // Both the author and the already-subscribed bystander must observe it.
    for ws in [&mut author_ws, &mut other_ws] {
        match recv(ws).await {
            ServerMsg::NewMessage {
                channel_id,
                author_id,
                content,
                ..
            } => {
                assert_eq!(channel_id, channel);
                assert_eq!(author_id, Some(author));
                assert_eq!(content, "ping");
            }
            other => panic!("expected NewMessage, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn send_message_by_non_member_is_forbidden() {
    let pool = setup_pool().await;
    let owner = seed_user(&pool).await;
    let (_server, channel) = seed_server_with_channel(&pool, owner).await;

    // A real, registered user who simply isn't a member of the server.
    let outsider = seed_user(&pool).await;
    let url = spawn_server(pool.clone()).await;
    let mut ws = connect_authed(&url, outsider).await;

    send(
        &mut ws,
        &ClientMsg::SendMessage {
            channel_id: channel,
            content: "intruder".into(),
        },
    )
    .await;

    match recv(&mut ws).await {
        ServerMsg::Error { code, message } => {
            assert_eq!(code, ErrorCode::Forbidden);
            assert_eq!(message, "not a member of this server");
        }
        other => panic!("expected Forbidden error, got {other:?}"),
    }
}

#[tokio::test]
async fn send_message_to_unknown_channel_is_not_found() {
    let pool = setup_pool().await;
    let author = seed_user(&pool).await;
    // Seed a real server/channel so the member is genuine, then aim at a
    // channel id that doesn't exist.
    let (_server, _channel) = seed_server_with_channel(&pool, author).await;

    let url = spawn_server(pool.clone()).await;
    let mut ws = connect_authed(&url, author).await;

    send(
        &mut ws,
        &ClientMsg::SendMessage {
            channel_id: Uuid::new_v4(),
            content: "into the void".into(),
        },
    )
    .await;

    match recv(&mut ws).await {
        ServerMsg::Error { code, message } => {
            assert_eq!(code, ErrorCode::NotFound);
            assert_eq!(message, "channel not found");
        }
        other => panic!("expected NotFound error, got {other:?}"),
    }
}

#[tokio::test]
async fn send_blank_message_is_rejected_before_channel_lookup() {
    let pool = setup_pool().await;
    let author = seed_user(&pool).await;

    // No server or channel is seeded, so the channel id below doesn't exist.
    // If content validation ran after the channel lookup we'd get NotFound;
    // getting BadRequest instead proves validation runs first.
    let url = spawn_server(pool.clone()).await;
    let mut ws = connect_authed(&url, author).await;

    send(
        &mut ws,
        &ClientMsg::SendMessage {
            channel_id: Uuid::new_v4(),
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
async fn send_oversized_message_is_rejected() {
    let pool = setup_pool().await;
    let author = seed_user(&pool).await;
    let (_server, channel) = seed_server_with_channel(&pool, author).await;

    let url = spawn_server(pool.clone()).await;
    let mut ws = connect_authed(&url, author).await;

    // One past the 4000-char cap; the WS path must surface the same rejection
    // the validator unit-tests cover.
    let too_long = "a".repeat(4001);
    send(
        &mut ws,
        &ClientMsg::SendMessage {
            channel_id: channel,
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
