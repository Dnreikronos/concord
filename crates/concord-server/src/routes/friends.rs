use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use concord_shared::protocol::ServerMsg;
use concord_shared::types::{Friend, FriendRequests, UserStatus, UserSummary};
use concord_shared::validation::ValidationError;

use crate::db;
use crate::error::AppError;
use crate::extractors::AuthUser;
use crate::state::AppState;

#[derive(Deserialize)]
struct SendFriendRequest {
    user_id: Uuid,
}

/// The result of `POST /api/friends/requests`. `outcome` is `"requested"` when a
/// new pending request was created (`201`), or `"accepted"` when the target had
/// already requested the caller and the two are now friends (`200`). `user` is
/// the other party either way, so the client can update its lists without a
/// refetch.
#[derive(Serialize)]
struct SendFriendResponse {
    outcome: &'static str,
    /// The friendship row id (the request id, or the now-accepted friendship).
    id: Uuid,
    user: UserSummary,
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(list_friends))
        .route("/{user_id}", delete(remove_friend))
        .route("/requests", get(list_requests).post(send_request))
        .route("/requests/{id}/accept", post(accept_request))
        .route("/requests/{id}", delete(delete_request))
}

/// `GET /api/friends` — the caller's accepted friends, alphabetically, each with
/// its live presence overlaid from the presence store.
async fn list_friends(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
) -> Result<Json<Vec<Friend>>, AppError> {
    let rows = db::list_friends(&state.pool, auth.user_id).await?;

    let ids: Vec<Uuid> = rows.iter().map(|(u, _)| u.id).collect();
    let statuses: HashMap<Uuid, UserStatus> = state
        .presence
        .get_many(&ids)
        .await
        .into_iter()
        .map(|p| (p.user_id, p.status))
        .collect();

    let friends = rows
        .into_iter()
        .map(|(user, since)| Friend {
            status: statuses.get(&user.id).copied().unwrap_or(UserStatus::Offline),
            user,
            since,
        })
        .collect();

    Ok(Json(friends))
}

/// `GET /api/friends/requests` — the caller's pending requests, incoming and
/// outgoing.
async fn list_requests(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
) -> Result<Json<FriendRequests>, AppError> {
    let requests = db::list_friend_requests(&state.pool, auth.user_id).await?;
    Ok(Json(requests))
}

/// `POST /api/friends/requests` — send a friend request to `user_id`, or accept
/// the target's pending request to the caller if one exists.
async fn send_request(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Json(req): Json<SendFriendRequest>,
) -> Result<(StatusCode, Json<SendFriendResponse>), AppError> {
    if req.user_id == auth.user_id {
        return Err(AppError::Validation(ValidationError::InvalidValue {
            field: "user_id",
            reason: "cannot send a friend request to yourself",
        }));
    }

    let user = db::get_user_summary(&state.pool, req.user_id)
        .await?
        .ok_or(AppError::NotFound)?;
    // The caller's own summary, for the event pushed to the other party.
    let me = db::get_user_summary(&state.pool, auth.user_id)
        .await?
        .ok_or_else(|| AppError::Internal("authenticated user not found".into()))?;

    match db::send_friend_request(&state.pool, auth.user_id, req.user_id).await? {
        db::SendFriendOutcome::Sent { id, .. } => {
            state
                .hub
                .send_to_user(req.user_id, &ServerMsg::FriendRequestReceived { request_id: id, from: me });
            Ok((StatusCode::CREATED, Json(SendFriendResponse { outcome: "requested", id, user })))
        }
        db::SendFriendOutcome::Accepted { id } => {
            // The target had already requested the caller, so this resolved to an
            // accept: tell them the caller is now their friend.
            state
                .hub
                .send_to_user(req.user_id, &ServerMsg::FriendRequestAccepted { user: me });
            Ok((StatusCode::OK, Json(SendFriendResponse { outcome: "accepted", id, user })))
        }
        db::SendFriendOutcome::AlreadyRequested | db::SendFriendOutcome::AlreadyFriends => {
            Err(AppError::FriendshipExists)
        }
    }
}

/// `POST /api/friends/requests/{id}/accept` — accept an incoming request. Only
/// the addressee may accept; anything else (missing, already resolved, or not
/// addressed to the caller) is reported as not found. Returns the new friend.
async fn accept_request(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(request_id): Path<Uuid>,
) -> Result<Json<Friend>, AppError> {
    let requester_id = db::accept_friend_request(&state.pool, request_id, auth.user_id)
        .await?
        .ok_or(AppError::NotFound)?;

    let user = db::get_user_summary(&state.pool, requester_id)
        .await?
        .ok_or(AppError::NotFound)?;

    // Tell the original requester their request was accepted; the caller is the
    // new friend.
    let me = db::get_user_summary(&state.pool, auth.user_id)
        .await?
        .ok_or_else(|| AppError::Internal("authenticated user not found".into()))?;
    state
        .hub
        .send_to_user(requester_id, &ServerMsg::FriendRequestAccepted { user: me });

    let status = state
        .presence
        .get_many(&[requester_id])
        .await
        .into_iter()
        .next()
        .map(|p| p.status)
        .unwrap_or(UserStatus::Offline);

    Ok(Json(Friend { user, status, since: Utc::now() }))
}

/// `DELETE /api/friends/requests/{id}` — reject an incoming request or cancel an
/// outgoing one (either party to a pending request may delete it).
async fn delete_request(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(request_id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let Some((requester_id, addressee_id)) =
        db::delete_friend_request(&state.pool, request_id, auth.user_id).await?
    else {
        return Err(AppError::NotFound);
    };

    // Notify the other party so their pending list drops this request.
    let other = if requester_id == auth.user_id { addressee_id } else { requester_id };
    state
        .hub
        .send_to_user(other, &ServerMsg::FriendRequestCanceled { request_id });

    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/friends/{user_id}` — unfriend `user_id` (either side may).
async fn remove_friend(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(user_id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    if db::remove_friend(&state.pool, auth.user_id, user_id).await? {
        state
            .hub
            .send_to_user(user_id, &ServerMsg::FriendRemoved { user_id: auth.user_id });
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}
