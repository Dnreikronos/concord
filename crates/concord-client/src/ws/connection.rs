use std::time::Duration;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use rand::{thread_rng, Rng};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::warn;
use uuid::Uuid;

use concord_shared::protocol::{ClientMsg, ErrorCode, ServerMsg, Token};

use super::types::{ConnState, WsCommand, WsEvent};

const OUTGOING_BUFFER_CAP: usize = 1024;
const AUTH_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a connection must stay up after authenticating before we trust it
/// enough to reset the reconnect backoff. Without this, a server that accepts
/// auth and then immediately drops the connection would be hammered in a tight
/// reconnect loop with no escalation.
const CONNECTION_STABLE_THRESHOLD: Duration = Duration::from_secs(5);

type ClientWsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
type WsSink = SplitSink<ClientWsStream, Message>;
type WsSource = SplitStream<ClientWsStream>;

/// Result of the connect + authenticate handshake.
enum Handshake {
    /// Authenticated; the live socket halves are handed back to the run loop.
    Ready {
        sink: WsSink,
        stream: WsSource,
        user_id: Uuid,
    },
    /// Transport-level failure; retry with backoff.
    Retry(String),
    /// The server rejected authentication; do not retry.
    AuthFailed { code: ErrorCode, message: String },
}

/// Outcome of attempting to serialize and write one message to the sink.
enum SinkSend {
    Sent,
    /// Message could not be serialized; it is dropped.
    Serialize,
    /// The sink is closed/errored.
    Closed,
}

async fn send_msg(sink: &mut WsSink, msg: &ClientMsg) -> SinkSend {
    let text = match serde_json::to_string(msg) {
        Ok(t) => t,
        Err(_) => return SinkSend::Serialize,
    };
    if sink.send(Message::Text(text.into())).await.is_err() {
        SinkSend::Closed
    } else {
        SinkSend::Sent
    }
}

/// Queue a message for delivery after the next successful connect, dropping it
/// if the buffer is already full.
fn buffer_outgoing(buf: &mut Vec<ClientMsg>, msg: ClientMsg) {
    if buf.len() < OUTGOING_BUFFER_CAP {
        buf.push(msg);
    } else {
        warn!("outgoing buffer full, dropping message");
    }
}

/// Re-queue a message that failed mid-send at the front of the buffer so it is
/// retried first after reconnecting, preserving send order.
fn rebuffer_front(buf: &mut Vec<ClientMsg>, msg: ClientMsg) {
    if buf.len() < OUTGOING_BUFFER_CAP {
        buf.insert(0, msg);
    } else {
        warn!("outgoing buffer full, dropping in-flight message");
    }
}

struct Backoff {
    attempt: u32,
    base_ms: u64,
    max_ms: u64,
}

impl Backoff {
    fn new() -> Self {
        Self {
            attempt: 0,
            base_ms: 500,
            max_ms: 30_000,
        }
    }

    fn reset(&mut self) {
        self.attempt = 0;
    }

    fn attempt(&self) -> u32 {
        self.attempt
    }

    fn next_delay(&mut self) -> Duration {
        let delay_ms = self.base_ms.saturating_mul(1u64 << self.attempt.min(6));
        let capped = delay_ms.min(self.max_ms);
        let jitter = thread_rng().gen_range(0..=capped / 4);
        self.attempt = self.attempt.saturating_add(1);
        Duration::from_millis(capped + jitter)
    }
}

pub(crate) async fn run(mut cmd_rx: mpsc::Receiver<WsCommand>, evt_tx: mpsc::Sender<WsEvent>) {
    let mut state = ConnState::Disconnected;
    let mut url: Option<String> = None;
    let mut token: Option<Token> = None;
    let mut outgoing_buffer: Vec<ClientMsg> = Vec::new();
    let mut backoff = Backoff::new();

    'outer: loop {
        match state {
            ConnState::Disconnected => match cmd_rx.recv().await {
                Some(WsCommand::Connect { url: u, token: t }) => {
                    url = Some(u);
                    token = Some(t);
                    state = ConnState::Connecting;
                }
                Some(WsCommand::Send(msg)) => buffer_outgoing(&mut outgoing_buffer, msg),
                Some(WsCommand::Shutdown) | None => {
                    let _ = evt_tx.send(WsEvent::Closed).await;
                    return;
                }
            },

            ConnState::Connecting => {
                let ws_url = url
                    .as_ref()
                    .expect("url set before entering Connecting")
                    .clone();
                let auth_token =
                    token.clone().expect("token set before entering Connecting");

                // Run the connect + auth handshake as a cancellable future so the
                // run loop stays responsive to commands (notably Shutdown) instead
                // of blocking on it for up to the auth-response timeout.
                let handshake = async move {
                    let (ws_stream, _) = match tokio_tungstenite::connect_async(&ws_url).await {
                        Ok(ok) => ok,
                        Err(e) => return Handshake::Retry(e.to_string()),
                    };
                    let (mut sink, mut stream) = ws_stream.split();

                    let auth_msg = ClientMsg::Authenticate { token: auth_token };
                    match send_msg(&mut sink, &auth_msg).await {
                        SinkSend::Sent => {}
                        SinkSend::Serialize => return Handshake::Retry("serialize error".into()),
                        SinkSend::Closed => {
                            return Handshake::Retry("send failed during auth".into())
                        }
                    }

                    let auth_response = match timeout(AUTH_RESPONSE_TIMEOUT, async {
                        loop {
                            match stream.next().await {
                                Some(Ok(Message::Text(t))) => return Ok(t),
                                Some(Ok(Message::Close(_))) | None => {
                                    return Err("connection closed during auth".to_owned());
                                }
                                Some(Err(e)) => return Err(e.to_string()),
                                Some(Ok(
                                    Message::Ping(_)
                                    | Message::Pong(_)
                                    | Message::Binary(_)
                                    | Message::Frame(_),
                                )) => continue,
                            }
                        }
                    })
                    .await
                    {
                        Ok(Ok(t)) => t,
                        Ok(Err(reason)) => return Handshake::Retry(reason),
                        Err(_) => return Handshake::Retry("auth response timeout".into()),
                    };

                    match serde_json::from_str::<ServerMsg>(&auth_response) {
                        Ok(ServerMsg::Authenticated { user_id }) => Handshake::Ready {
                            sink,
                            stream,
                            user_id,
                        },
                        Ok(ServerMsg::Error { code, message }) => {
                            Handshake::AuthFailed { code, message }
                        }
                        _ => Handshake::AuthFailed {
                            code: ErrorCode::Internal,
                            message: "unexpected auth response".into(),
                        },
                    }
                };
                tokio::pin!(handshake);

                let (mut sink, mut stream, user_id) = 'connect: loop {
                    tokio::select! {
                        outcome = &mut handshake => match outcome {
                            Handshake::Ready { sink, stream, user_id } => {
                                break 'connect (sink, stream, user_id);
                            }
                            Handshake::Retry(reason) => {
                                let _ = evt_tx.send(WsEvent::Disconnected { reason }).await;
                                state = ConnState::Reconnecting;
                                continue 'outer;
                            }
                            Handshake::AuthFailed { code, message } => {
                                let _ = evt_tx.send(WsEvent::AuthFailed { code, message }).await;
                                state = ConnState::Disconnected;
                                continue 'outer;
                            }
                        },
                        cmd = cmd_rx.recv() => match cmd {
                            Some(WsCommand::Send(msg)) => buffer_outgoing(&mut outgoing_buffer, msg),
                            Some(WsCommand::Connect { url: u, token: t }) => {
                                url = Some(u);
                                token = Some(t);
                                backoff.reset();
                                continue 'outer;
                            }
                            Some(WsCommand::Shutdown) | None => {
                                let _ = evt_tx.send(WsEvent::Closed).await;
                                return;
                            }
                        },
                    }
                };

                let _ = evt_tx.send(WsEvent::Connected { user_id }).await;

                // Defer the backoff reset: only trust the connection once it has
                // survived a stable interval, so a server that flaps right after
                // auth still escalates the reconnect delay.
                let stable_timer = tokio::time::sleep(CONNECTION_STABLE_THRESHOLD);
                tokio::pin!(stable_timer);
                let mut backoff_reset_pending = true;

                // Drain buffered messages, preserving any that don't make it out
                // if the sink dies mid-drain.
                let mut pending = std::mem::take(&mut outgoing_buffer);
                let mut idx = 0;
                let mut drain_closed = false;
                while idx < pending.len() {
                    match send_msg(&mut sink, &pending[idx]).await {
                        SinkSend::Sent | SinkSend::Serialize => idx += 1,
                        SinkSend::Closed => {
                            drain_closed = true;
                            break;
                        }
                    }
                }
                if drain_closed {
                    outgoing_buffer = pending.split_off(idx);
                    let _ = evt_tx
                        .send(WsEvent::Disconnected {
                            reason: "send failed draining buffer".into(),
                        })
                        .await;
                    state = ConnState::Reconnecting;
                    continue 'outer;
                }

                loop {
                    tokio::select! {
                        _ = &mut stable_timer, if backoff_reset_pending => {
                            backoff.reset();
                            backoff_reset_pending = false;
                        }
                        frame = stream.next() => {
                            match frame {
                                Some(Ok(Message::Text(text))) => {
                                    match serde_json::from_str::<ServerMsg>(&text) {
                                        Ok(server_msg) => {
                                            let _ = evt_tx.send(WsEvent::Message(server_msg)).await;
                                        }
                                        Err(e) => {
                                            warn!(error = %e, "malformed ServerMsg, dropping frame");
                                        }
                                    }
                                }
                                Some(Ok(Message::Close(_))) | None => {
                                    let _ = evt_tx.send(WsEvent::Disconnected {
                                        reason: "server closed connection".into(),
                                    }).await;
                                    state = ConnState::Reconnecting;
                                    continue 'outer;
                                }
                                Some(Err(e)) => {
                                    let _ = evt_tx.send(WsEvent::Disconnected {
                                        reason: e.to_string(),
                                    }).await;
                                    state = ConnState::Reconnecting;
                                    continue 'outer;
                                }
                                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_))) => {}
                            }
                        }
                        cmd = cmd_rx.recv() => {
                            match cmd {
                                Some(WsCommand::Send(msg)) => {
                                    match send_msg(&mut sink, &msg).await {
                                        SinkSend::Sent => {}
                                        SinkSend::Serialize => {
                                            warn!("dropping unserializable outgoing message");
                                        }
                                        SinkSend::Closed => {
                                            rebuffer_front(&mut outgoing_buffer, msg);
                                            let _ = evt_tx.send(WsEvent::Disconnected {
                                                reason: "send failed".into(),
                                            }).await;
                                            state = ConnState::Reconnecting;
                                            continue 'outer;
                                        }
                                    }
                                }
                                Some(WsCommand::Connect { url: u, token: t }) => {
                                    url = Some(u);
                                    token = Some(t);
                                    let _ = sink.close().await;
                                    backoff.reset();
                                    state = ConnState::Connecting;
                                    continue 'outer;
                                }
                                Some(WsCommand::Shutdown) | None => {
                                    let _ = sink.close().await;
                                    let _ = evt_tx.send(WsEvent::Closed).await;
                                    return;
                                }
                            }
                        }
                    }
                }
            }

            ConnState::Reconnecting => {
                let attempt = backoff.attempt();
                let _ = evt_tx.send(WsEvent::Reconnecting { attempt }).await;
                let delay = backoff.next_delay();

                tokio::select! {
                    _ = tokio::time::sleep(delay) => {
                        state = ConnState::Connecting;
                    }
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(WsCommand::Connect { url: u, token: t }) => {
                                url = Some(u);
                                token = Some(t);
                                backoff.reset();
                                state = ConnState::Connecting;
                            }
                            Some(WsCommand::Send(msg)) => buffer_outgoing(&mut outgoing_buffer, msg),
                            Some(WsCommand::Shutdown) | None => {
                                let _ = evt_tx.send(WsEvent::Closed).await;
                                return;
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Backoff;
    use std::time::Duration;

    fn assert_in_range(delay: Duration, base_ms: u64) {
        let ms = delay.as_millis() as u64;
        assert!(ms >= base_ms, "delay {ms}ms < base {base_ms}ms");
        assert!(ms <= base_ms + base_ms / 4, "delay {ms}ms > base {base_ms}ms + 25% jitter");
    }

    #[test]
    fn backoff_progression() {
        let mut b = Backoff::new();
        assert_in_range(b.next_delay(), 500);
        assert_in_range(b.next_delay(), 1000);
        assert_in_range(b.next_delay(), 2000);
        assert_in_range(b.next_delay(), 4000);
        assert_in_range(b.next_delay(), 8000);
    }

    #[test]
    fn backoff_caps_at_max() {
        let mut b = Backoff::new();
        for _ in 0..20 {
            b.next_delay();
        }
        assert_in_range(b.next_delay(), 30_000);
    }

    #[test]
    fn backoff_resets() {
        let mut b = Backoff::new();
        b.next_delay();
        b.next_delay();
        b.reset();
        assert_eq!(b.attempt(), 0);
        assert_in_range(b.next_delay(), 500);
    }
}
