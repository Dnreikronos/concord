use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::Response;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use tokio::task::JoinHandle;
use tokio::time::{timeout_at, Instant};
use uuid::Uuid;

use concord_shared::protocol::{ClientMsg, ErrorCode, ServerMsg};
use concord_shared::types::UserStatus;
use concord_shared::validation::validate_message_content;

use secrecy::ExposeSecret;
use tracing::warn;

use crate::db;
use crate::state::AppState;

/// Outcome of decoding one inbound WebSocket frame into a `ClientMsg`.
enum Frame {
    /// A well-formed client message.
    Msg(ClientMsg),
    /// The peer sent a Close frame.
    Close,
    /// A control or binary frame with no protocol meaning here; ignore it.
    Skip,
    /// A text frame that failed to parse as a `ClientMsg`.
    Invalid,
}

/// Decode a raw WebSocket frame into a protocol-level message.
///
/// Shared by the pre-auth and post-auth read loops so the frame → text →
/// parse-`ClientMsg` handling stays in one place.
fn parse_client_frame(frame: Message) -> Frame {
    match frame {
        Message::Text(text) => match serde_json::from_str::<ClientMsg>(&text) {
            Ok(msg) => Frame::Msg(msg),
            Err(_) => Frame::Invalid,
        },
        Message::Close(_) => Frame::Close,
        _ => Frame::Skip,
    }
}

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> Response {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

type Sink = Arc<tokio::sync::Mutex<SplitSink<WebSocket, Message>>>;

/// Everything the post-auth loop needs to tear down cleanly once the socket
/// closes.
struct Session {
    uid: Uuid,
    conn_id: Uuid,
    /// Drains the user's outbound queue onto the socket.
    fwd_handle: JoinHandle<()>,
    /// Re-arms the Redis presence TTL while the connection lives. `None` when
    /// presence persistence is disabled.
    heartbeat_handle: Option<JoinHandle<()>>,
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    let (sender, mut receiver) = socket.split();
    let sender: Sink = Arc::new(tokio::sync::Mutex::new(sender));

    let Some(session) = wait_for_auth(&sender, &mut receiver, &state).await else {
        return;
    };

    handle_authenticated(session, sender, receiver, state).await;
}

async fn wait_for_auth(
    sender: &Sink,
    receiver: &mut futures_util::stream::SplitStream<WebSocket>,
    state: &Arc<AppState>,
) -> Option<Session> {
    // Single deadline for the whole handshake. Re-arming a fresh timeout each
    // loop iteration let a client trickle non-text frames every <timeout and
    // stay unauthenticated forever; a fixed deadline bounds that regardless of
    // how many junk frames arrive.
    let deadline = Instant::now() + state.ws_auth_timeout;

    loop {
        let frame = match timeout_at(deadline, receiver.next()).await {
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

        let client_msg = match parse_client_frame(frame) {
            Frame::Msg(msg) => msg,
            Frame::Close => return None,
            Frame::Skip => continue,
            Frame::Invalid => {
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
                let (conn_id, rx, is_first) = state.hub.register(uid);
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

                init_presence(state, sender, uid, is_first).await;

                let fwd = spawn_forwarder(rx, Arc::clone(sender));
                let heartbeat = spawn_heartbeat(state, uid);
                return Some(Session {
                    uid,
                    conn_id,
                    fwd_handle: fwd,
                    heartbeat_handle: heartbeat,
                });
            }
            Err(msg) => {
                let _ = send_error(sender, ErrorCode::Unauthorized, &msg).await;
                return None;
            }
        }
    }
}

async fn handle_authenticated(
    session: Session,
    sender: Sink,
    mut receiver: futures_util::stream::SplitStream<WebSocket>,
    state: Arc<AppState>,
) {
    let Session {
        uid,
        conn_id,
        fwd_handle,
        heartbeat_handle,
    } = session;

    while let Some(Ok(frame)) = receiver.next().await {
        let client_msg = match parse_client_frame(frame) {
            Frame::Msg(msg) => msg,
            Frame::Close => break,
            Frame::Skip => continue,
            Frame::Invalid => {
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

            ClientMsg::SendDirectMessage {
                dm_channel_id,
                content,
            } => {
                if let Err(e) = validate_message_content(&content) {
                    let _ =
                        send_error(&sender, ErrorCode::BadRequest, &e.to_string())
                            .await;
                    continue;
                }

                // Load the DM's members once: the list both authorizes the
                // sender and is the fan-out set. A non-member — which also
                // covers a non-existent channel, since that has no members — is
                // rejected before any insert. We answer Forbidden rather than
                // NotFound so the endpoint never confirms a DM the caller can't
                // see. Fanning out to this same set reaches every connected
                // member (author included) without connect-time subscription
                // bookkeeping.
                let members = match db::list_dm_member_ids(&state.pool, dm_channel_id).await {
                    Ok(members) => members,
                    Err(e) => {
                        warn!(user_id = %uid, dm_channel_id = %dm_channel_id, error = ?e, "failed to load DM members");
                        let _ = send_error(&sender, ErrorCode::Internal, "internal error").await;
                        continue;
                    }
                };
                if !members.contains(&uid) {
                    let _ = send_error(
                        &sender,
                        ErrorCode::Forbidden,
                        "not a member of this DM",
                    )
                    .await;
                    continue;
                }

                // DM messages live in the same `messages` table, keyed by the
                // dm_channel id (see migration 0005).
                let inserted = match db::insert_message(
                    &state.pool,
                    dm_channel_id,
                    uid,
                    &content,
                )
                .await
                {
                    Ok(row) => row,
                    Err(e) => {
                        warn!(user_id = %uid, dm_channel_id = %dm_channel_id, error = ?e, "failed to insert DM message");
                        let _ = send_error(&sender, ErrorCode::Internal, "internal error").await;
                        continue;
                    }
                };

                let msg = ServerMsg::NewDirectMessage {
                    id: inserted.id,
                    dm_channel_id,
                    author_id: Some(uid),
                    content,
                };
                for member in members {
                    state.hub.send_to_user(member, &msg);
                }
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
                // Clear any live typing session so leavers don't linger as a
                // stuck indicator until the sweeper catches up.
                state.typing.stop(uid, channel_id).await;
            }

            ClientMsg::StartTyping { channel_id } => {
                if verify_channel_membership(&state, &sender, channel_id, uid)
                    .await
                    .is_none()
                {
                    continue;
                }

                state.typing.start(uid, channel_id).await;
            }

            ClientMsg::StopTyping { channel_id } => {
                state.typing.stop(uid, channel_id).await;
            }

            ClientMsg::UpdateStatus { status } => {
                state.presence.set(uid, status).await;
                broadcast_status_change(&state, uid, status).await;
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

    let was_last = state.hub.unregister(uid, conn_id);
    fwd_handle.abort();
    if let Some(handle) = heartbeat_handle {
        handle.abort();
    }
    if was_last {
        state.presence.clear(uid).await;
        broadcast_status_change(&state, uid, UserStatus::Offline).await;
    }
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

/// On-connect presence setup. On the user's first connection it marks them
/// online (persisting to Redis and notifying shared-server peers); on every
/// connection it sends back a snapshot of those peers' current presence so the
/// client starts with an accurate roster.
///
/// Best-effort throughout: if the peer lookup fails the whole step is skipped
/// rather than failing the connection, which also keeps the auth handshake
/// testable against a pool that never connects.
async fn init_presence(state: &Arc<AppState>, sender: &Sink, uid: Uuid, is_first: bool) {
    let peers = match db::list_shared_server_user_ids(&state.pool, uid).await {
        Ok(peers) => peers,
        Err(e) => {
            warn!(user_id = %uid, error = ?e, "failed to load presence peers");
            return;
        }
    };

    if is_first {
        state.presence.set(uid, UserStatus::Online).await;
        let msg = ServerMsg::UserStatusChanged {
            user_id: uid,
            status: UserStatus::Online,
        };
        for &peer in &peers {
            state.hub.send_to_user(peer, &msg);
        }
    }

    let users = state.presence.get_many(&peers).await;
    let _ = send_msg(sender, &ServerMsg::PresenceSnapshot { users }).await;
}

/// Notify every user who shares a server with `uid` that their status changed.
/// The hub only delivers to peers with a live connection; offline peers are a
/// no-op.
async fn broadcast_status_change(state: &AppState, uid: Uuid, status: UserStatus) {
    let peers = match db::list_shared_server_user_ids(&state.pool, uid).await {
        Ok(peers) => peers,
        Err(e) => {
            warn!(user_id = %uid, error = ?e, "failed to load presence peers for broadcast");
            return;
        }
    };
    let msg = ServerMsg::UserStatusChanged {
        user_id: uid,
        status,
    };
    for peer in peers {
        state.hub.send_to_user(peer, &msg);
    }
}

/// Spawn a task that re-arms the user's Redis presence TTL at half the TTL
/// interval, so a connected user never lapses to offline. Returns `None` when
/// presence persistence is disabled — nothing to keep alive.
fn spawn_heartbeat(state: &Arc<AppState>, uid: Uuid) -> Option<JoinHandle<()>> {
    if !state.presence.is_enabled() {
        return None;
    }
    let presence = state.presence.clone();
    let period = (presence.ttl() / 2).max(Duration::from_secs(1));
    Some(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(period);
        ticker.tick().await; // the first tick resolves immediately; skip it
        loop {
            ticker.tick().await;
            presence.refresh(uid).await;
        }
    }))
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
