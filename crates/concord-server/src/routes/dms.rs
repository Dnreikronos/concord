use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, post};
use axum::{Json, Router};
use serde::Deserialize;
use uuid::Uuid;

use concord_shared::types::DmChannel;
use concord_shared::validation::{validate_dm_name, ValidationError, DM_GROUP_MAX, DM_GROUP_MIN};

use crate::db;
use crate::error::AppError;
use crate::extractors::AuthUser;
use crate::state::AppState;

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
        .route("/", post(create_group_dm))
        .route("/{id}/members", post(add_member))
        .route("/{id}/members/{user_id}", delete(remove_member))
}

/// `POST /api/dms` — create a group DM owned by the caller.
///
/// The caller is always a participant; `recipient_ids` lists the others. The
/// list is de-duplicated and the caller is stripped from it if present, so the
/// 2–10 participant bound is checked against the real head count.
async fn create_group_dm(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Json(req): Json<CreateGroupDmRequest>,
) -> Result<(StatusCode, Json<DmChannel>), AppError> {
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

    Ok((StatusCode::CREATED, Json(channel)))
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

    if db::is_dm_member(&state.pool, dm_channel_id, req.user_id).await? {
        return Err(AppError::AlreadyDmMember);
    }

    if db::dm_member_count(&state.pool, dm_channel_id).await? as usize >= DM_GROUP_MAX {
        return Err(AppError::Validation(ValidationError::InvalidValue {
            field: "user_id",
            reason: "a group DM allows at most 10 participants",
        }));
    }

    db::insert_dm_member(&state.pool, dm_channel_id, req.user_id).await?;

    Ok(StatusCode::NO_CONTENT)
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
