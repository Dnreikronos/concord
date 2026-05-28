use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::Response;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use tokio::time::timeout;
use uuid::Uuid;

use concord_shared::protocol::{ClientMsg, ErrorCode, ServerMsg};
use concord_shared::validation::validate_message_content;

use secrecy::ExposeSecret;
use tracing::warn;

use crate::db;
use crate::state::AppState;

const AUTH_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> Response {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

type Sink = Arc<tokio::sync::Mutex<SplitSink<WebSocket, Message>>>;

async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    let (sender, mut receiver) = socket.split();
    let sender: Sink = Arc::new(tokio::sync::Mutex::new(sender));

    let Some((uid, conn_id, fwd_handle)) =
        wait_for_auth(&sender, &mut receiver, &state).await
    else {
        return;
    };

    handle_authenticated(uid, conn_id, sender, receiver, fwd_handle, state).await;
}

async fn wait_for_auth(
    sender: &Sink,
    receiver: &mut futures_util::stream::SplitStream<WebSocket>,
    state: &Arc<AppState>,
) -> Option<(Uuid, Uuid, tokio::task::JoinHandle<()>)> {
    loop {
        let frame = match timeout(AUTH_TIMEOUT, receiver.next()).await {
            Ok(Some(Ok(frame))) => frame,
            Err(_) => {
                let _ = send_error(sender, ErrorCode::Unauthorized, "auth timeout")
                    .await;
                return None;
            }
            Ok(Some(Err(_))) => {
                let _ = send_error(sender, ErrorCode::Internal, "websocket error during auth")
                    .await;
                return None;
            }
            Ok(None) => {
                return None;
            }
        };

        let text = match frame {
            Message::Text(t) => t,
            Message::Close(_) => return None,
            _ => continue,
        };

        let client_msg: ClientMsg = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(_) => {
                let _ = send_error(sender, ErrorCode::BadRequest, "invalid message format")
                    .await;
                return None;
            }
        };

        let ClientMsg::Authenticate { token } = client_msg else {
            let _ = send_error(sender, ErrorCode::Unauthorized, "must authenticate first")
                .await;
            return None;
        };

        match authenticate(state, token.as_str()).await {
            Ok(uid) => {
                let (conn_id, rx) = state.hub.register(uid);
                let _ = send_msg(sender, &ServerMsg::Authenticated { user_id: uid }).await;

                match db::list_channel_ids_for_user(&state.pool, uid).await {
                    Ok(channel_ids) => {
                        for ch in channel_ids {
                            state.hub.subscribe(uid, ch);
                        }
                    }
                    Err(e) => {
                        warn!(user_id = %uid, error = ?e, "failed to load channel subscriptions");
                    }
                }

                let fwd = spawn_forwarder(rx, Arc::clone(sender));
                return Some((uid, conn_id, fwd));
            }
            Err(msg) => {
                let _ = send_error(sender, ErrorCode::Unauthorized, &msg).await;
                return None;
            }
        }
    }
}

async fn handle_authenticated(
    uid: Uuid,
    conn_id: Uuid,
    sender: Sink,
    mut receiver: futures_util::stream::SplitStream<WebSocket>,
    fwd_handle: tokio::task::JoinHandle<()>,
    state: Arc<AppState>,
) {
    while let Some(Ok(frame)) = receiver.next().await {
        let text = match frame {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };

        let client_msg: ClientMsg = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(_) => {
                let _ = send_error(
                    &sender,
                    ErrorCode::BadRequest,
                    "invalid message format",
                )
                .await;
                continue;
            }
        };

        match client_msg {
            ClientMsg::Authenticate { .. } => {
                let _ = send_error(
                    &sender,
                    ErrorCode::BadRequest,
                    "already authenticated",
                )
                .await;
            }

            ClientMsg::SendMessage {
                channel_id,
                content,
            } => {
                if let Err(e) = validate_message_content(&content) {
                    let _ =
                        send_error(&sender, ErrorCode::BadRequest, &e.to_string())
                            .await;
                    continue;
                }

                if verify_channel_membership(&state, &sender, channel_id, uid)
                    .await
                    .is_none()
                {
                    continue;
                }

                let inserted = match db::insert_message(
                    &state.pool,
                    channel_id,
                    uid,
                    &content,
                )
                .await
                {
                    Ok(row) => row,
                    Err(_) => {
                        let _ = send_error(&sender, ErrorCode::Internal, "internal error").await;
                        continue;
                    }
                };

                state.hub.broadcast_to_channel(
                    channel_id,
                    &ServerMsg::NewMessage {
                        id: inserted.id,
                        channel_id,
                        author_id: Some(uid),
                        content,
                    },
                );
            }

            ClientMsg::EditMessage {
                message_id,
                content,
            } => {
                if let Err(e) = validate_message_content(&content) {
                    let _ =
                        send_error(&sender, ErrorCode::BadRequest, &e.to_string())
                            .await;
                    continue;
                }

                let channel_id =
                    match db::get_message_channel(&state.pool, message_id).await {
                        Ok(Some(ch)) => ch,
                        Ok(None) => {
                            let _ = send_error(
                                &sender,
                                ErrorCode::NotFound,
                                "message not found",
                            )
                            .await;
                            continue;
                        }
                        Err(_) => {
                            let _ = send_error(
                                &sender,
                                ErrorCode::Internal,
                                "internal error",
                            )
                            .await;
                            continue;
                        }
                    };

                if verify_channel_membership(&state, &sender, channel_id, uid)
                    .await
                    .is_none()
                {
                    continue;
                }

                match db::update_message_if_author(
                    &state.pool,
                    message_id,
                    uid,
                    &content,
                )
                .await
                {
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        let _ = send_error(
                            &sender,
                            ErrorCode::Forbidden,
                            "not the author",
                        )
                        .await;
                        continue;
                    }
                    Err(_) => {
                        let _ = send_error(
                            &sender,
                            ErrorCode::Internal,
                            "internal error",
                        )
                        .await;
                        continue;
                    }
                }

                state.hub.broadcast_to_channel(
                    channel_id,
                    &ServerMsg::MessageEdited {
                        message_id,
                        content,
                    },
                );
            }

            ClientMsg::DeleteMessage { message_id } => {
                if let Some(channel_id) =
                    try_delete_message(&state, message_id, uid).await
                {
                    state.hub.broadcast_to_channel(
                        channel_id,
                        &ServerMsg::MessageDeleted { message_id },
                    );
                } else {
                    let _ = send_error(
                        &sender,
                        ErrorCode::Forbidden,
                        "message not found, or not the author/admin",
                    )
                    .await;
                }
            }

            ClientMsg::JoinChannel { channel_id } => {
                if verify_channel_membership(&state, &sender, channel_id, uid)
                    .await
                    .is_none()
                {
                    continue;
                }

                state.hub.subscribe(uid, channel_id);
            }

            ClientMsg::LeaveChannel { channel_id } => {
                state.hub.unsubscribe(uid, channel_id);
            }

            ClientMsg::StartTyping { channel_id } => {
                if verify_channel_membership(&state, &sender, channel_id, uid)
                    .await
                    .is_none()
                {
                    continue;
                }

                if !state.hub.check_typing_cooldown(uid, channel_id) {
                    continue;
                }

                state.hub.broadcast_to_channel(
                    channel_id,
                    &ServerMsg::UserTyping {
                        channel_id,
                        user_id: uid,
                    },
                );
            }

            _ => {
                let _ = send_error(
                    &sender,
                    ErrorCode::BadRequest,
                    "unsupported message type",
                )
                .await;
            }
        }
    }

    state.hub.unregister(uid, conn_id);
    fwd_handle.abort();
}

async fn verify_channel_membership(
    state: &AppState,
    sender: &Sink,
    channel_id: Uuid,
    user_id: Uuid,
) -> Option<Uuid> {
    let server_id = match db::get_channel_server(&state.pool, channel_id).await {
        Ok(Some(sid)) => sid,
        Ok(None) => {
            let _ =
                send_error(sender, ErrorCode::NotFound, "channel not found").await;
            return None;
        }
        Err(_) => {
            let _ =
                send_error(sender, ErrorCode::Internal, "internal error").await;
            return None;
        }
    };

    match db::is_server_member(&state.pool, server_id, user_id).await {
        Ok(true) => {}
        Ok(false) => {
            let _ = send_error(
                sender,
                ErrorCode::Forbidden,
                "not a member of this server",
            )
            .await;
            return None;
        }
        Err(_) => {
            let _ =
                send_error(sender, ErrorCode::Internal, "internal error").await;
            return None;
        }
    }

    Some(server_id)
}

/// Try author-delete first; on failure check admin privileges and force-delete.
async fn try_delete_message(
    state: &AppState,
    message_id: Uuid,
    user_id: Uuid,
) -> Option<Uuid> {
    if let Ok(Some(channel_id)) =
        db::delete_message_if_author(&state.pool, message_id, user_id).await
    {
        return Some(channel_id);
    }

    let channel_id = db::get_message_channel(&state.pool, message_id)
        .await
        .ok()??;
    let server_id = db::get_channel_server(&state.pool, channel_id)
        .await
        .ok()??;

    if !db::is_server_admin(&state.pool, server_id, user_id)
        .await
        .unwrap_or(false)
    {
        return None;
    }

    db::delete_message(&state.pool, message_id).await.ok()?
}

fn spawn_forwarder(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<ServerMsg>,
    sender: Sink,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let text = match serde_json::to_string(&msg) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let mut sink = sender.lock().await;
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    })
}

async fn authenticate(state: &AppState, token: &str) -> Result<Uuid, String> {
    let claims =
        crate::jwt::decode_access_token(token, state.jwt_secret.expose_secret())
            .map_err(|e| e.to_string())?;
    Ok(claims.sub)
}

async fn send_msg(sender: &Sink, msg: &ServerMsg) -> Result<(), ()> {
    let text = serde_json::to_string(msg).map_err(|_| ())?;
    sender
        .lock()
        .await
        .send(Message::Text(text.into()))
        .await
        .map_err(|_| ())
}

async fn send_error(sender: &Sink, code: ErrorCode, message: &str) -> Result<(), ()> {
    send_msg(
        sender,
        &ServerMsg::Error {
            code,
            message: message.to_owned(),
        },
    )
    .await
}
