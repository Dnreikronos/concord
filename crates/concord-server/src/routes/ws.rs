use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::Response;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use uuid::Uuid;

use concord_shared::protocol::{ClientMsg, ErrorCode, ServerMsg};
use concord_shared::validation::validate_message_content;

use crate::db;
use crate::state::AppState;

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> Response {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    let (sender, mut receiver) = socket.split();
    let sender = Arc::new(tokio::sync::Mutex::new(sender));

    let mut user_id: Option<Uuid> = None;
    let mut broadcast_rx = state.tx.subscribe();

    let fwd_sender = Arc::clone(&sender);
    let fwd_handle = tokio::spawn(async move {
        while let Ok((_, msg)) = broadcast_rx.recv().await {
            let text = match serde_json::to_string(&msg) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let mut sink = fwd_sender.lock().await;
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

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
            ClientMsg::Authenticate { token } => {
                match authenticate(&state, token.as_str()).await {
                    Ok(uid) => {
                        user_id = Some(uid);
                        let _ = send_msg(
                            &sender,
                            &ServerMsg::Authenticated { user_id: uid },
                        )
                        .await;
                    }
                    Err(msg) => {
                        let _ =
                            send_error(&sender, ErrorCode::Unauthorized, &msg).await;
                    }
                }
            }

            ClientMsg::EditMessage {
                message_id,
                content,
            } => {
                let Some(uid) = user_id else {
                    let _ = send_error(
                        &sender,
                        ErrorCode::Unauthorized,
                        "not authenticated",
                    )
                    .await;
                    continue;
                };

                if let Err(e) = validate_message_content(&content) {
                    let _ =
                        send_error(&sender, ErrorCode::BadRequest, &e.to_string())
                            .await;
                    continue;
                }

                let author = match db::get_message_author(&state.pool, message_id).await
                {
                    Ok(Some(a)) => a,
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

                if author != uid {
                    let _ = send_error(
                        &sender,
                        ErrorCode::Forbidden,
                        "not the message author",
                    )
                    .await;
                    continue;
                }

                if db::update_message_content(&state.pool, message_id, &content)
                    .await
                    .is_err()
                {
                    let _ =
                        send_error(&sender, ErrorCode::Internal, "internal error")
                            .await;
                    continue;
                }

                let channel_id =
                    db::get_message_channel(&state.pool, message_id).await;
                let channel_id = match channel_id {
                    Ok(Some(id)) => id,
                    _ => continue,
                };

                let _ = state.tx.send((
                    channel_id,
                    ServerMsg::MessageEdited {
                        message_id,
                        content,
                    },
                ));
            }

            ClientMsg::DeleteMessage { message_id } => {
                let Some(uid) = user_id else {
                    let _ = send_error(
                        &sender,
                        ErrorCode::Unauthorized,
                        "not authenticated",
                    )
                    .await;
                    continue;
                };

                let channel_id =
                    match db::get_message_channel(&state.pool, message_id).await {
                        Ok(Some(id)) => id,
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

                let is_author =
                    match db::get_message_author(&state.pool, message_id).await {
                        Ok(Some(a)) => a == uid,
                        Ok(None) => false,
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

                let is_admin = if !is_author {
                    match db::get_channel_server(&state.pool, channel_id).await {
                        Ok(Some(server_id)) => {
                            db::is_server_admin(&state.pool, uid, server_id)
                                .await
                                .unwrap_or(false)
                        }
                        _ => false,
                    }
                } else {
                    false
                };

                if !is_author && !is_admin {
                    let _ = send_error(
                        &sender,
                        ErrorCode::Forbidden,
                        "not the author or an admin",
                    )
                    .await;
                    continue;
                }

                if db::delete_message(&state.pool, message_id).await.is_err() {
                    let _ =
                        send_error(&sender, ErrorCode::Internal, "internal error")
                            .await;
                    continue;
                }

                let _ = state.tx.send((
                    channel_id,
                    ServerMsg::MessageDeleted { message_id },
                ));
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

    fwd_handle.abort();
}

async fn authenticate(_state: &AppState, _token: &str) -> Result<Uuid, String> {
    // TODO: validate JWT / session token and return the user_id
    Err("authentication not yet implemented".into())
}

async fn send_msg(
    sender: &Arc<tokio::sync::Mutex<SplitSink<WebSocket, Message>>>,
    msg: &ServerMsg,
) -> Result<(), ()> {
    let text = serde_json::to_string(msg).map_err(|_| ())?;
    sender
        .lock()
        .await
        .send(Message::Text(text.into()))
        .await
        .map_err(|_| ())
}

async fn send_error(
    sender: &Arc<tokio::sync::Mutex<SplitSink<WebSocket, Message>>>,
    code: ErrorCode,
    message: &str,
) -> Result<(), ()> {
    send_msg(
        sender,
        &ServerMsg::Error {
            code,
            message: message.to_owned(),
        },
    )
    .await
}
