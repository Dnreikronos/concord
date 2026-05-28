use concord_client::ws::{ConnectionHandle, WsEvent};
use concord_shared::protocol::Token;

#[tokio::main]
async fn main() {
    let (handle, mut events) = ConnectionHandle::spawn(256);

    let url = std::env::var("CONCORD_WS_URL")
        .unwrap_or_else(|_| "ws://127.0.0.1:3000/ws".into());
    let token =
        std::env::var("CONCORD_TOKEN").expect("CONCORD_TOKEN env var required");

    handle.connect(url, Token::new(token)).await;

    while let Some(event) = events.recv().await {
        match event {
            WsEvent::Connected { user_id } => {
                eprintln!("connected as {user_id}");
            }
            WsEvent::Message(msg) => {
                eprintln!("received: {msg:?}");
            }
            WsEvent::Disconnected { reason } => {
                eprintln!("disconnected: {reason}");
            }
            WsEvent::Reconnecting { attempt } => {
                eprintln!("reconnecting (attempt {attempt})...");
            }
            WsEvent::AuthFailed { code, message } => {
                eprintln!("auth failed: {code:?} - {message}");
                break;
            }
            WsEvent::Closed => break,
        }
    }
}
