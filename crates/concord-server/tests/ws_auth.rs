//! Integration tests for the auth-first WebSocket handshake (issue #15).
//!
//! These drive the real `ws_handler` over a loopback TCP socket but never touch
//! Postgres: the auth-rejection paths return before any query runs, and the
//! post-auth channel load failure is non-fatal, so a lazily-constructed bogus
//! pool is enough. That keeps the headline behavior testable without a DB.

use std::sync::Arc;
use std::time::Duration;

use concord_server::hub::Hub;
use concord_server::jwt;
use concord_server::routes;
use concord_server::state::AppState;
use concord_shared::protocol::{ClientMsg, ErrorCode, ServerMsg, Token};

use futures_util::{SinkExt, StreamExt};
use sqlx::postgres::PgPoolOptions;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use uuid::Uuid;

const JWT_SECRET: &str = "test-secret-do-not-use-in-prod";

type ClientWs = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Bind the real router over loopback and return the `ws://.../ws` URL.
async fn spawn_server(auth_timeout: Duration) -> String {
    // A lazy pool that never successfully connects. The auth paths under test
    // either never query or tolerate a query failure.
    let pool = PgPoolOptions::new()
        .acquire_timeout(Duration::from_secs(1))
        .connect_lazy("postgres://postgres@127.0.0.1:1/concord_nonexistent")
        .expect("lazy pool construction should not fail");

    let state = Arc::new(AppState {
        pool,
        hub: Arc::new(Hub::new()),
        jwt_secret: secrecy::SecretString::from(JWT_SECRET),
        github_oauth: None,
        google_oauth: None,
        http_client: reqwest::Client::new(),
        ws_auth_timeout: auth_timeout,
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
/// bounded by a timeout so a hung test fails fast.
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

#[tokio::test]
async fn idle_connection_times_out() {
    let url = spawn_server(Duration::from_millis(300)).await;
    let mut ws = connect(&url).await;

    // Send nothing; the single deadline must fire and reject us.
    match recv(&mut ws).await {
        ServerMsg::Error { code, message } => {
            assert_eq!(code, ErrorCode::Unauthorized);
            assert_eq!(message, "auth timeout");
        }
        other => panic!("expected auth-timeout error, got {other:?}"),
    }
}

#[tokio::test]
async fn trickling_non_text_frames_still_times_out() {
    // Regression guard for the re-armed-timeout bug: pinging every <timeout must
    // not keep the connection alive past the single deadline.
    let url = spawn_server(Duration::from_millis(400)).await;
    let ws = connect(&url).await;
    let (mut sink, mut stream) = ws.split();

    let pinger = async {
        loop {
            if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                break;
            }
            // Faster than the auth timeout — the old code would reset on each.
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };

    let recv_err = async {
        loop {
            match stream.next().await {
                Some(Ok(Message::Text(t))) => {
                    return serde_json::from_str::<ServerMsg>(&t).unwrap()
                }
                Some(Ok(_)) => continue,
                other => panic!("expected text frame, got {other:?}"),
            }
        }
    };

    let result = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::select! {
            _ = pinger => unreachable!("pinger only ends on send error"),
            msg = recv_err => msg,
        }
    })
    .await
    .expect("connection should have been closed by the auth deadline");

    match result {
        ServerMsg::Error { code, .. } => assert_eq!(code, ErrorCode::Unauthorized),
        other => panic!("expected auth-timeout error, got {other:?}"),
    }
}

#[tokio::test]
async fn non_auth_first_message_is_rejected() {
    let url = spawn_server(Duration::from_secs(10)).await;
    let mut ws = connect(&url).await;

    send(
        &mut ws,
        &ClientMsg::SendMessage {
            channel_id: Uuid::new_v4(),
            content: "hi before auth".into(),
        },
    )
    .await;

    match recv(&mut ws).await {
        ServerMsg::Error { code, message } => {
            assert_eq!(code, ErrorCode::Unauthorized);
            assert_eq!(message, "must authenticate first");
        }
        other => panic!("expected must-authenticate error, got {other:?}"),
    }
}

#[tokio::test]
async fn malformed_first_frame_is_rejected() {
    let url = spawn_server(Duration::from_secs(10)).await;
    let mut ws = connect(&url).await;

    ws.send(Message::Text("not json".into())).await.unwrap();

    match recv(&mut ws).await {
        ServerMsg::Error { code, message } => {
            assert_eq!(code, ErrorCode::BadRequest);
            assert_eq!(message, "invalid message format");
        }
        other => panic!("expected bad-request error, got {other:?}"),
    }
}

#[tokio::test]
async fn duplicate_authenticate_after_auth_is_rejected() {
    let url = spawn_server(Duration::from_secs(10)).await;
    let mut ws = connect(&url).await;

    let uid = Uuid::new_v4();
    let token = jwt::encode_access_token(uid, JWT_SECRET).unwrap();

    send(&mut ws, &ClientMsg::Authenticate { token: Token::new(token) }).await;
    match recv(&mut ws).await {
        ServerMsg::Authenticated { user_id } => assert_eq!(user_id, uid),
        other => panic!("expected Authenticated, got {other:?}"),
    }

    // Second auth on an already-authenticated connection.
    let token2 = jwt::encode_access_token(uid, JWT_SECRET).unwrap();
    send(&mut ws, &ClientMsg::Authenticate { token: Token::new(token2) }).await;
    match recv(&mut ws).await {
        ServerMsg::Error { code, message } => {
            assert_eq!(code, ErrorCode::BadRequest);
            assert_eq!(message, "already authenticated");
        }
        other => panic!("expected already-authenticated error, got {other:?}"),
    }
}
