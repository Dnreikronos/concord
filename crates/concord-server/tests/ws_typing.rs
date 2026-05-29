//! End-to-end WebSocket typing-indicator tests (issue #19).
//!
//! Drives the real `ws_handler` over loopback against a Postgres test database
//! (`DATABASE_URL`). Covers the `StartTyping`/`StopTyping` fan-out, sender
//! exclusion, and the auto-expire sweeper (using a short TTL so the test is
//! quick). The cross-instance Redis path is covered separately by the unit
//! tests; here the transport is in-process.

use std::sync::Arc;
use std::time::Duration;

use concord_server::hub::Hub;
use concord_server::presence::Presence;
use concord_server::state::AppState;
use concord_server::typing::Typing;
use concord_server::{db, jwt, routes};
use concord_shared::protocol::{ClientMsg, ServerMsg, Token};

use futures_util::{SinkExt, StreamExt};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

const JWT_SECRET: &str = "test-secret-do-not-use-in-prod";

type ClientWs = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct Fixture {
    url: String,
    token_a: String,
    token_b: String,
    user_a: Uuid,
    channel_id: Uuid,
}

async fn build_pool() -> PgPool {
    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set for integration tests");
    let pool = PgPoolOptions::new()
        .max_connections(3)
        .connect(&database_url)
        .await
        .expect("failed to connect to test database");
    sqlx::migrate!("../../migrations")
        .run(&pool)
        .await
        .expect("failed to run migrations");
    pool
}

fn uniq(prefix: &str) -> String {
    format!("{prefix}{}", &Uuid::new_v4().simple().to_string()[..12])
}

fn email() -> String {
    format!("{}@typing.example.com", Uuid::new_v4().simple())
}

/// Two members (A, B) of one server sharing a text channel, behind a freshly
/// bound server using `ttl` for typing sessions and a fast sweeper.
async fn setup(ttl: Duration) -> Fixture {
    let pool = build_pool().await;

    let user_a = db::insert_user(&pool, &uniq("a"), &email(), "x").await.unwrap();
    let user_b = db::insert_user(&pool, &uniq("b"), &email(), "x").await.unwrap();
    let server = db::insert_server(&pool, "typing-test", None, user_a.id).await.unwrap();
    db::insert_server_member(&pool, server.id, user_a.id, "admin").await.unwrap();
    db::insert_server_member(&pool, server.id, user_b.id, "member").await.unwrap();
    let channel = db::insert_channel(&pool, server.id, "general", None, "text").await.unwrap();

    let token_a = jwt::encode_access_token(user_a.id, JWT_SECRET).unwrap();
    let token_b = jwt::encode_access_token(user_b.id, JWT_SECRET).unwrap();

    let hub = Arc::new(Hub::new());
    let typing = Arc::new(Typing::new(Arc::clone(&hub), ttl, None));
    Arc::clone(&typing).spawn_sweeper(Duration::from_millis(50));

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

    Fixture {
        url: format!("ws://{addr}/ws"),
        token_a,
        token_b,
        user_a: user_a.id,
        channel_id: channel.id,
    }
}

async fn send(ws: &mut ClientWs, msg: &ClientMsg) {
    let text = serde_json::to_string(msg).unwrap();
    ws.send(Message::Text(text.into())).await.unwrap();
}

/// Read frames until a `ServerMsg` text frame arrives, bounded by a timeout.
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

/// Like [`recv`] but returns `None` if no message arrives within `dur` — used
/// to assert the *absence* of a frame (e.g. the sender's own echo).
async fn recv_within(ws: &mut ClientWs, dur: Duration) -> Option<ServerMsg> {
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
    tokio::time::timeout(dur, fut).await.ok()
}

/// Connect, authenticate, and wait for the `Authenticated` ack.
async fn connect_auth(url: &str, token: &str) -> ClientWs {
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    send(&mut ws, &ClientMsg::Authenticate { token: Token::new(token) }).await;
    match recv(&mut ws).await {
        ServerMsg::Authenticated { .. } => {}
        other => panic!("expected Authenticated, got {other:?}"),
    }
    ws
}

/// Both clients are auto-subscribed to their channels during the handshake; the
/// ack is sent before the subscribe completes server-side, so give it a beat to
/// settle before relying on channel membership.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(200)).await;
}

#[tokio::test]
async fn start_typing_reaches_other_member_but_not_sender() {
    let fx = setup(Duration::from_secs(5)).await;
    let mut a = connect_auth(&fx.url, &fx.token_a).await;
    let mut b = connect_auth(&fx.url, &fx.token_b).await;
    settle().await;

    send(&mut a, &ClientMsg::StartTyping { channel_id: fx.channel_id }).await;

    match recv(&mut b).await {
        ServerMsg::TypingStarted { channel_id, user_id } => {
            assert_eq!(channel_id, fx.channel_id);
            assert_eq!(user_id, fx.user_a);
        }
        other => panic!("B expected TypingStarted, got {other:?}"),
    }
    assert!(
        recv_within(&mut a, Duration::from_millis(300)).await.is_none(),
        "sender must not receive her own typing indicator"
    );
}

#[tokio::test]
async fn stop_typing_reaches_other_member() {
    let fx = setup(Duration::from_secs(5)).await;
    let mut a = connect_auth(&fx.url, &fx.token_a).await;
    let mut b = connect_auth(&fx.url, &fx.token_b).await;
    settle().await;

    send(&mut a, &ClientMsg::StartTyping { channel_id: fx.channel_id }).await;
    assert!(matches!(recv(&mut b).await, ServerMsg::TypingStarted { .. }));

    send(&mut a, &ClientMsg::StopTyping { channel_id: fx.channel_id }).await;
    match recv(&mut b).await {
        ServerMsg::TypingStopped { channel_id, user_id } => {
            assert_eq!(channel_id, fx.channel_id);
            assert_eq!(user_id, fx.user_a);
        }
        other => panic!("B expected TypingStopped, got {other:?}"),
    }
}

#[tokio::test]
async fn typing_auto_expires_after_ttl() {
    let fx = setup(Duration::from_millis(300)).await;
    let mut a = connect_auth(&fx.url, &fx.token_a).await;
    let mut b = connect_auth(&fx.url, &fx.token_b).await;
    settle().await;

    send(&mut a, &ClientMsg::StartTyping { channel_id: fx.channel_id }).await;
    assert!(matches!(recv(&mut b).await, ServerMsg::TypingStarted { .. }));

    // No re-send: the session lapses and the sweeper emits a stop.
    match recv(&mut b).await {
        ServerMsg::TypingStopped { channel_id, user_id } => {
            assert_eq!(channel_id, fx.channel_id);
            assert_eq!(user_id, fx.user_a);
        }
        other => panic!("B expected TypingStopped on expiry, got {other:?}"),
    }
}
