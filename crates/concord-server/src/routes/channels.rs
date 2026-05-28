use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::patch;
use axum::{Json, Router};
use serde::Deserialize;
use uuid::Uuid;

use concord_shared::types::{Channel, ChannelType};
use concord_shared::validation::{validate_channel_name, validate_channel_topic};

use crate::db;
use crate::error::AppError;
use crate::extractors::AuthUser;
use crate::state::AppState;

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

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/{id}", patch(update_channel).delete(delete_channel))
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
    if let Some(ref topic) = req.topic {
        validate_channel_topic(topic)?;
    }

    let channel_type_str = match req.channel_type {
        ChannelType::Text => "text",
        ChannelType::Voice => "voice",
    };

    let position = db::next_channel_position(&state.pool, server_id).await?;
    let channel = db::insert_channel(
        &state.pool,
        server_id,
        name,
        req.topic.as_deref(),
        channel_type_str,
        position,
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
    if let Some(Some(ref topic)) = req.topic {
        validate_channel_topic(topic)?;
    }

    let topic_ref = req.topic.as_ref().map(|o| o.as_deref());

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
