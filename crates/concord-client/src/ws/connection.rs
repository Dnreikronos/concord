use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::warn;

use concord_shared::protocol::{ClientMsg, ErrorCode, ServerMsg, Token};

use super::types::{ConnState, WsCommand, WsEvent};

const OUTGOING_BUFFER_CAP: usize = 1024;

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
        self.attempt = self.attempt.saturating_add(1);
        Duration::from_millis(capped)
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
                Some(WsCommand::Send(msg)) => {
                    if outgoing_buffer.len() < OUTGOING_BUFFER_CAP {
                        outgoing_buffer.push(msg);
                    } else {
                        warn!("outgoing buffer full, dropping message");
                    }
                }
                Some(WsCommand::Shutdown) | None => {
                    let _ = evt_tx.send(WsEvent::Closed).await;
                    return;
                }
            },

            ConnState::Connecting => {
                let ws_url = url.as_ref().unwrap();
                match tokio_tungstenite::connect_async(ws_url).await {
                    Ok((ws_stream, _)) => {
                        let (mut sink, mut stream) = ws_stream.split();

                        let auth_msg = ClientMsg::Authenticate {
                            token: token.clone().unwrap(),
                        };
                        let text = match serde_json::to_string(&auth_msg) {
                            Ok(t) => t,
                            Err(_) => {
                                let _ = evt_tx
                                    .send(WsEvent::Disconnected {
                                        reason: "serialize error".into(),
                                    })
                                    .await;
                                state = ConnState::Reconnecting;
                                continue 'outer;
                            }
                        };
                        if sink.send(Message::Text(text.into())).await.is_err() {
                            let _ = evt_tx
                                .send(WsEvent::Disconnected {
                                    reason: "send failed during auth".into(),
                                })
                                .await;
                            state = ConnState::Reconnecting;
                            continue 'outer;
                        }

                        let auth_response = loop {
                            match stream.next().await {
                                Some(Ok(Message::Text(t))) => break t,
                                Some(Ok(Message::Close(_))) | None => {
                                    let _ = evt_tx
                                        .send(WsEvent::Disconnected {
                                            reason: "connection closed during auth".into(),
                                        })
                                        .await;
                                    state = ConnState::Reconnecting;
                                    continue 'outer;
                                }
                                Some(Err(e)) => {
                                    let _ = evt_tx
                                        .send(WsEvent::Disconnected {
                                            reason: e.to_string(),
                                        })
                                        .await;
                                    state = ConnState::Reconnecting;
                                    continue 'outer;
                                }
                                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_))) => continue,
                            }
                        };

                        match serde_json::from_str::<ServerMsg>(&auth_response) {
                            Ok(ServerMsg::Authenticated { user_id }) => {
                                backoff.reset();
                                let _ =
                                    evt_tx.send(WsEvent::Connected { user_id }).await;
                            }
                            Ok(ServerMsg::Error { code, message }) => {
                                let _ =
                                    evt_tx.send(WsEvent::AuthFailed { code, message }).await;
                                state = ConnState::Disconnected;
                                continue 'outer;
                            }
                            _ => {
                                let _ = evt_tx
                                    .send(WsEvent::AuthFailed {
                                        code: ErrorCode::Internal,
                                        message: "unexpected auth response".into(),
                                    })
                                    .await;
                                state = ConnState::Disconnected;
                                continue 'outer;
                            }
                        }

                        // -- Connected: drain buffer then select loop --
                        for msg in outgoing_buffer.drain(..) {
                            let text = match serde_json::to_string(&msg) {
                                Ok(t) => t,
                                Err(_) => continue,
                            };
                            if sink.send(Message::Text(text.into())).await.is_err() {
                                let _ = evt_tx
                                    .send(WsEvent::Disconnected {
                                        reason: "send failed draining buffer".into(),
                                    })
                                    .await;
                                state = ConnState::Reconnecting;
                                continue 'outer;
                            }
                        }

                        loop {
                            tokio::select! {
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
                                            let text = match serde_json::to_string(&msg) {
                                                Ok(t) => t,
                                                Err(_) => continue,
                                            };
                                            if sink.send(Message::Text(text.into())).await.is_err() {
                                                let _ = evt_tx.send(WsEvent::Disconnected {
                                                    reason: "send failed".into(),
                                                }).await;
                                                state = ConnState::Reconnecting;
                                                continue 'outer;
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
                    Err(e) => {
                        let _ = evt_tx
                            .send(WsEvent::Disconnected {
                                reason: e.to_string(),
                            })
                            .await;
                        state = ConnState::Reconnecting;
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
                            Some(WsCommand::Send(msg)) => {
                                if outgoing_buffer.len() < OUTGOING_BUFFER_CAP {
                                    outgoing_buffer.push(msg);
                                } else {
                                    warn!("outgoing buffer full, dropping message");
                                }
                            }
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

    #[test]
    fn backoff_progression() {
        let mut b = Backoff::new();
        assert_eq!(b.next_delay(), Duration::from_millis(500));
        assert_eq!(b.next_delay(), Duration::from_millis(1000));
        assert_eq!(b.next_delay(), Duration::from_millis(2000));
        assert_eq!(b.next_delay(), Duration::from_millis(4000));
        assert_eq!(b.next_delay(), Duration::from_millis(8000));
    }

    #[test]
    fn backoff_caps_at_max() {
        let mut b = Backoff::new();
        for _ in 0..20 {
            b.next_delay();
        }
        assert_eq!(b.next_delay(), Duration::from_millis(30_000));
    }

    #[test]
    fn backoff_resets() {
        let mut b = Backoff::new();
        b.next_delay();
        b.next_delay();
        b.reset();
        assert_eq!(b.attempt(), 0);
        assert_eq!(b.next_delay(), Duration::from_millis(500));
    }
}
