use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, patch};
use axum::{Json, Router};
use serde::Deserialize;
use uuid::Uuid;

use concord_shared::types::{Channel, ChannelType, MessageWithAuthor};
use concord_shared::validation::{validate_channel_name, validate_channel_topic};

use crate::db::{self, ChannelAccess};
use crate::error::AppError;
use crate::extractors::AuthUser;
use crate::state::AppState;

/// Default page size when `?limit` is omitted.
const DEFAULT_MESSAGE_LIMIT: i64 = 50;
/// Upper bound on `?limit`; larger values are clamped down.
const MAX_MESSAGE_LIMIT: i64 = 100;

#[derive(Deserialize)]
pub struct CreateChannelRequest {
    name: String,
    #[serde(default)]
    topic: Option<String>,
    #[serde(default = "default_channel_type")]
    channel_type: ChannelType,
}

fn default_channel_type() -> ChannelType {
    ChannelType::Text
}

#[derive(Deserialize)]
struct UpdateChannelRequest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    topic: Option<Option<String>>,
}

fn deserialize_optional_field<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::deserialize(deserializer)?))
}

#[derive(Deserialize)]
struct MessageHistoryQuery {
    /// Return messages strictly older than this message id (the cursor).
    #[serde(default)]
    before: Option<Uuid>,
    /// Page size; clamped to `[1, MAX_MESSAGE_LIMIT]`, defaults to
    /// `DEFAULT_MESSAGE_LIMIT`.
    #[serde(default)]
    limit: Option<i64>,
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/{id}", patch(update_channel).delete(delete_channel))
        .route("/{id}/messages", get(list_messages))
}

/// `GET /api/channels/{id}/messages` — cursor-paginated history, newest first.
///
/// Works for both server channels and DM channels; access is gated on
/// membership of whichever kind `{id}` names.
async fn list_messages(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(channel_id): Path<Uuid>,
    Query(query): Query<MessageHistoryQuery>,
) -> Result<Json<Vec<MessageWithAuthor>>, AppError> {
    match db::check_channel_read_access(&state.pool, channel_id, auth.user_id).await? {
        ChannelAccess::Authorized => {}
        ChannelAccess::Forbidden => return Err(AppError::Forbidden),
        ChannelAccess::NotFound => return Err(AppError::NotFound),
    }

    let limit = query
        .limit
        .unwrap_or(DEFAULT_MESSAGE_LIMIT)
        .clamp(1, MAX_MESSAGE_LIMIT);

    let messages =
        db::list_channel_messages(&state.pool, channel_id, query.before, limit).await?;

    Ok(Json(messages))
}

pub async fn create_channel(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(server_id): Path<Uuid>,
    Json(req): Json<CreateChannelRequest>,
) -> Result<(StatusCode, Json<Channel>), AppError> {
    if !db::is_server_admin(&state.pool, server_id, auth.user_id).await? {
        if !db::server_exists(&state.pool, server_id).await? {
            return Err(AppError::NotFound);
        }
        return Err(AppError::Forbidden);
    }

    let name = req.name.trim();
    validate_channel_name(name)?;
    let topic = req.topic.as_deref().map(str::trim);
    if let Some(t) = topic {
        validate_channel_topic(t)?;
    }

    let channel_type_str = match req.channel_type {
        ChannelType::Text => "text",
        ChannelType::Voice => "voice",
    };

    let channel = db::insert_channel(
        &state.pool,
        server_id,
        name,
        topic,
        channel_type_str,
    )
    .await?;

    Ok((StatusCode::CREATED, Json(channel)))
}

pub async fn list_channels(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(server_id): Path<Uuid>,
) -> Result<Json<Vec<Channel>>, AppError> {
    if !db::is_server_member(&state.pool, server_id, auth.user_id).await? {
        if !db::server_exists(&state.pool, server_id).await? {
            return Err(AppError::NotFound);
        }
        return Err(AppError::Forbidden);
    }

    let channels = db::list_channels_for_server(&state.pool, server_id).await?;
    Ok(Json(channels))
}

async fn update_channel(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(channel_id): Path<Uuid>,
    Json(req): Json<UpdateChannelRequest>,
) -> Result<Json<Channel>, AppError> {
    if req.name.is_none() && req.topic.is_none() {
        return Err(AppError::Validation(
            concord_shared::validation::ValidationError::BlankContent {
                field: "request body",
            },
        ));
    }

    let trimmed_name = req.name.as_deref().map(str::trim);
    if let Some(name) = trimmed_name {
        validate_channel_name(name)?;
    }
    let trimmed_topic = req.topic.as_ref().map(|o| o.as_deref().map(str::trim));
    if let Some(Some(t)) = trimmed_topic {
        validate_channel_topic(t)?;
    }

    let topic_ref = trimmed_topic;

    let channel = db::update_channel_if_admin(
        &state.pool,
        channel_id,
        auth.user_id,
        trimmed_name,
        topic_ref,
    )
    .await?;

    channel.map(Json).ok_or(AppError::NotFound)
}

async fn delete_channel(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(channel_id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let deleted =
        db::delete_channel_if_admin(&state.pool, channel_id, auth.user_id).await?;

    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}
