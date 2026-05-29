use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use uuid::Uuid;

use concord_shared::types::DmChannelInfo;
use concord_shared::validation::ValidationError;

use crate::db;
use crate::error::AppError;
use crate::extractors::AuthUser;
use crate::state::AppState;

#[derive(Deserialize)]
struct CreateDmRequest {
    recipient_id: Uuid,
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/", post(create_dm))
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
