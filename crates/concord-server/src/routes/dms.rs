use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use uuid::Uuid;

use concord_shared::types::{DmChannelInfo, DmConversation};
use concord_shared::validation::{validate_dm_name, ValidationError, DM_GROUP_MAX, DM_GROUP_MIN};

use crate::db;
use crate::error::AppError;
use crate::extractors::AuthUser;
use crate::state::AppState;

#[derive(Deserialize)]
struct CreateDmRequest {
    recipient_id: Uuid,
}

#[derive(Deserialize)]
struct CreateGroupDmRequest {
    recipient_ids: Vec<Uuid>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct AddMemberRequest {
    user_id: Uuid,
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(list_dms).post(create_dm))
        .route("/group", post(create_group_dm))
        .route("/{id}/members", post(add_member))
        .route("/{id}/members/{user_id}", delete(remove_member))
        .route("/{id}/read", post(mark_read))
}

/// `GET /api/dms` — list the caller's DM conversations, newest activity first.
///
/// Returns every 1:1 and group DM the caller belongs to, each with its
/// participants, member count, a preview of the latest message, and the
/// caller's unread flag. Ordering and unread semantics live in
/// [`db::list_dm_conversations`].
async fn list_dms(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
) -> Result<Json<Vec<DmConversation>>, AppError> {
    let conversations = db::list_dm_conversations(&state.pool, auth.user_id).await?;
    Ok(Json(conversations))
}

/// `POST /api/dms/{id}/read` — mark a DM read for the caller as of now.
///
/// Clears the conversation's unread flag until another member posts again.
/// A non-member — which also covers a non-existent channel, since that has no
/// members — gets `404 Not Found`, so the endpoint never confirms a DM the
/// caller can't see.
async fn mark_read(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(dm_channel_id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    if !db::is_dm_member(&state.pool, dm_channel_id, auth.user_id).await? {
        return Err(AppError::NotFound);
    }

    db::mark_dm_read(&state.pool, dm_channel_id, auth.user_id).await?;

    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/dms` — open (or reuse) a 1:1 DM with `recipient_id`.
///
/// Find-or-create: returns `201 Created` when a new channel is opened and
/// `200 OK` when an existing DM between the two users is reused. The response
/// body is the channel with both participants resolved either way.
async fn create_dm(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Json(req): Json<CreateDmRequest>,
) -> Result<(StatusCode, Json<DmChannelInfo>), AppError> {
    if req.recipient_id == auth.user_id {
        return Err(AppError::Validation(ValidationError::InvalidValue {
            field: "recipient_id",
            reason: "cannot open a DM with yourself",
        }));
    }

    if !db::user_exists(&state.pool, req.recipient_id).await? {
        return Err(AppError::NotFound);
    }

    let (channel, created) =
        db::find_or_create_dm_channel(&state.pool, auth.user_id, req.recipient_id).await?;

    let participants = db::list_dm_participants(&state.pool, channel.id).await?;

    let info = DmChannelInfo {
        id: channel.id,
        name: channel.name,
        is_group: channel.is_group,
        owner_id: channel.owner_id,
        created_at: channel.created_at,
        participants,
    };

    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };

    Ok((status, Json(info)))
}

/// `POST /api/dms/group` — create a group DM owned by the caller.
///
/// The caller is always a participant; `recipient_ids` lists the others. The
/// list is de-duplicated and the caller is stripped from it if present, so the
/// 2–10 participant bound is checked against the real head count.
async fn create_group_dm(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Json(req): Json<CreateGroupDmRequest>,
) -> Result<(StatusCode, Json<DmChannelInfo>), AppError> {
    // Names are optional; a blank/whitespace-only name is treated as "unnamed".
    let name: Option<String> = req
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    if let Some(ref n) = name {
        validate_dm_name(n)?;
    }

    let mut recipients: Vec<Uuid> = Vec::new();
    for id in req.recipient_ids {
        if id != auth.user_id && !recipients.contains(&id) {
            recipients.push(id);
        }
    }

    // Participant total counts the creator.
    let total = recipients.len() + 1;
    if total < DM_GROUP_MIN {
        return Err(AppError::Validation(ValidationError::InvalidValue {
            field: "recipient_ids",
            reason: "a group DM needs at least one other participant",
        }));
    }
    if total > DM_GROUP_MAX {
        return Err(AppError::Validation(ValidationError::InvalidValue {
            field: "recipient_ids",
            reason: "a group DM allows at most 10 participants",
        }));
    }

    // Every recipient must be a real account, or the dm_members insert would
    // fail mid-transaction on the foreign key.
    let existing = db::existing_user_ids(&state.pool, &recipients).await?;
    if existing.len() != recipients.len() {
        return Err(AppError::Validation(ValidationError::InvalidValue {
            field: "recipient_ids",
            reason: "one or more recipients do not exist",
        }));
    }

    let mut tx = state.pool.begin().await.map_err(|e| AppError::Internal(e.to_string()))?;

    let channel = db::insert_dm_channel(&mut *tx, name.as_deref(), auth.user_id).await?;
    db::insert_dm_member(&mut *tx, channel.id, auth.user_id).await?;
    for id in &recipients {
        db::insert_dm_member(&mut *tx, channel.id, *id).await?;
    }

    tx.commit().await.map_err(|e| AppError::Internal(e.to_string()))?;

    // Resolve participants so the group-create response matches the 1:1 path
    // (create_dm), which also returns a fully-populated DmChannelInfo.
    let participants = db::list_dm_participants(&state.pool, channel.id).await?;

    let info = DmChannelInfo {
        id: channel.id,
        name: channel.name,
        is_group: channel.is_group,
        owner_id: channel.owner_id,
        created_at: channel.created_at,
        participants,
    };

    Ok((StatusCode::CREATED, Json(info)))
}

/// `POST /api/dms/{id}/members` — add a user to an existing group DM.
///
/// Any current member may add others (only removal is owner-gated). Non-members
/// and 1:1 DMs are reported as not found so the endpoint never confirms a group
/// the caller can't see.
async fn add_member(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(dm_channel_id): Path<Uuid>,
    Json(req): Json<AddMemberRequest>,
) -> Result<StatusCode, AppError> {
    if db::get_group_dm(&state.pool, dm_channel_id).await?.is_none() {
        return Err(AppError::NotFound);
    }
    if !db::is_dm_member(&state.pool, dm_channel_id, auth.user_id).await? {
        return Err(AppError::NotFound);
    }

    if !db::user_exists(&state.pool, req.user_id).await? {
        return Err(AppError::Validation(ValidationError::InvalidValue {
            field: "user_id",
            reason: "user does not exist",
        }));
    }

    // Duplicate-check, cap-check, and insert run together behind a per-channel
    // advisory lock so concurrent adds can't both pass `count < max` and push
    // the group over DM_GROUP_MAX (or both insert the same user and 500 on the
    // PK). See db::add_dm_member_checked.
    match db::add_dm_member_checked(&state.pool, dm_channel_id, req.user_id, DM_GROUP_MAX).await? {
        db::AddMemberOutcome::Added => Ok(StatusCode::NO_CONTENT),
        db::AddMemberOutcome::AlreadyMember => Err(AppError::AlreadyDmMember),
        db::AddMemberOutcome::Full => Err(AppError::Validation(ValidationError::InvalidValue {
            field: "user_id",
            reason: "a group DM allows at most 10 participants",
        })),
    }
}

/// `DELETE /api/dms/{id}/members/{user_id}` — remove a member.
///
/// Removing yourself (leaving) is always allowed for a member; removing anyone
/// else is owner-only. Ownership transfer and empty-channel cleanup are handled
/// atomically in [`db::remove_dm_member`].
async fn remove_member(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path((dm_channel_id, target_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, AppError> {
    let channel = db::get_group_dm(&state.pool, dm_channel_id)
        .await?
        .ok_or(AppError::NotFound)?;

    if !db::is_dm_member(&state.pool, dm_channel_id, auth.user_id).await? {
        return Err(AppError::NotFound);
    }

    // Pulling someone else out is the owner's prerogative; leaving is your own.
    if target_id != auth.user_id && channel.owner_id != Some(auth.user_id) {
        return Err(AppError::Forbidden);
    }

    if !db::remove_dm_member(&state.pool, dm_channel_id, target_id).await? {
        return Err(AppError::NotFound);
    }

    Ok(StatusCode::NO_CONTENT)
}
