//! State-machine tests for the background WebSocket connection task, driven
//! through the public `ConnectionHandle` API against a minimal in-process
//! WebSocket server.

use std::time::Duration;

use concord_client::ws::{ConnectionHandle, WsEvent};
use concord_shared::protocol::{ClientMsg, ErrorCode, ServerMsg, Token};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

/// Spin up a one-shot WebSocket server that reads the client's first frame
/// (the `Authenticate`) and replies with `response`, then holds the connection
/// open until the client disconnects. Returns the `ws://` URL to dial.
async fn spawn_server(response: ServerMsg) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();

        // Consume the auth request the client sends first.
        let _ = ws.next().await;

        let text = serde_json::to_string(&response).unwrap();
        let _ = ws.send(Message::Text(text.into())).await;

        // Keep the socket open so a successful handshake stays "connected".
        while let Some(Ok(_)) = ws.next().await {}
    });

    format!("ws://{addr}/")
}

async fn next_event(events: &mut tokio::sync::mpsc::Receiver<WsEvent>) -> WsEvent {
    tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .expect("timed out waiting for event")
        .expect("event channel closed")
}

#[tokio::test]
async fn connect_then_authenticated_emits_connected() {
    let uid = Uuid::new_v4();
    let url = spawn_server(ServerMsg::Authenticated { user_id: uid }).await;

    let (handle, mut events) = ConnectionHandle::spawn(16);
    handle.connect(url, Token::new("dummy-token")).await.unwrap();

    match next_event(&mut events).await {
        WsEvent::Connected { user_id } => assert_eq!(user_id, uid),
        other => panic!("expected Connected, got {other:?}"),
    }
}

#[tokio::test]
async fn auth_error_emits_auth_failed() {
    let url = spawn_server(ServerMsg::Error {
        code: ErrorCode::Unauthorized,
        message: "bad token".into(),
    })
    .await;

    let (handle, mut events) = ConnectionHandle::spawn(16);
    handle.connect(url, Token::new("dummy-token")).await.unwrap();

    match next_event(&mut events).await {
        WsEvent::AuthFailed { code, .. } => assert_eq!(code, ErrorCode::Unauthorized),
        other => panic!("expected AuthFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn shutdown_while_disconnected_emits_closed() {
    let (handle, mut events) = ConnectionHandle::spawn(16);
    handle.shutdown().await.unwrap();

    match next_event(&mut events).await {
        WsEvent::Closed => {}
        other => panic!("expected Closed, got {other:?}"),
    }
}

#[tokio::test]
async fn buffered_send_is_delivered_after_connect() {
    // A message queued before connecting should be flushed once the handshake
    // completes. The server echoes nothing; we just assert it arrives.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<ClientMsg>();

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();

        // First frame is the auth request.
        let _ = ws.next().await;
        let resp = serde_json::to_string(&ServerMsg::Authenticated {
            user_id: Uuid::new_v4(),
        })
        .unwrap();
        let _ = ws.send(Message::Text(resp.into())).await;

        // Next frame should be the buffered SendMessage.
        if let Some(Ok(Message::Text(t))) = ws.next().await {
            let msg: ClientMsg = serde_json::from_str(&t).unwrap();
            let _ = tx.send(msg);
        }
        while let Some(Ok(_)) = ws.next().await {}
    });

    let channel_id = Uuid::new_v4();
    let (handle, _events) = ConnectionHandle::spawn(16);
    // Queue before connecting.
    handle
        .send(ClientMsg::SendMessage {
            channel_id,
            content: "hello".into(),
        })
        .await
        .unwrap();
    handle
        .connect(format!("ws://{addr}/"), Token::new("dummy-token"))
        .await
        .unwrap();

    let received = tokio::time::timeout(Duration::from_secs(2), rx)
        .await
        .expect("timed out waiting for buffered message")
        .expect("server task dropped sender");

    match received {
        ClientMsg::SendMessage { channel_id: ch, content } => {
            assert_eq!(ch, channel_id);
            assert_eq!(content, "hello");
        }
        other => panic!("expected buffered SendMessage, got {other:?}"),
    }
}
