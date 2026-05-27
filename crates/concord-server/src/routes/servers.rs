use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use uuid::Uuid;

use concord_shared::types::Server;
use concord_shared::validation::{validate_icon_url, validate_server_name};

use crate::db;
use crate::error::AppError;
use crate::extractors::AuthUser;
use crate::state::AppState;

#[derive(Deserialize)]
struct CreateServerRequest {
    name: String,
    #[serde(default)]
    icon_url: Option<String>,
}

#[derive(Deserialize)]
struct UpdateServerRequest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    icon_url: Option<Option<String>>,
}

fn deserialize_optional_field<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::deserialize(deserializer)?))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", post(create_server).get(list_servers))
        .route("/{id}", get(get_server).patch(update_server).delete(delete_server))
}

async fn create_server(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Json(req): Json<CreateServerRequest>,
) -> Result<(StatusCode, Json<Server>), AppError> {
    let name = req.name.trim();
    validate_server_name(name)?;
    if let Some(ref url) = req.icon_url {
        validate_icon_url(url)?;
    }

    let mut tx = state.pool.begin().await.map_err(|e| AppError::Internal(e.to_string()))?;

    let server = db::insert_server(&mut *tx, name, req.icon_url.as_deref(), auth.user_id).await?;
    db::insert_server_member(&mut *tx, server.id, auth.user_id, "admin").await?;
    db::insert_channel(&mut *tx, server.id, "general", "text", 0).await?;

    tx.commit().await.map_err(|e| AppError::Internal(e.to_string()))?;

    Ok((StatusCode::CREATED, Json(server)))
}

async fn list_servers(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
) -> Result<Json<Vec<Server>>, AppError> {
    let servers = db::list_servers_for_user(&state.pool, auth.user_id).await?;
    Ok(Json(servers))
}

async fn get_server(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(server_id): Path<Uuid>,
) -> Result<Json<Server>, AppError> {
    let is_member = db::is_server_member(&state.pool, server_id, auth.user_id).await?;
    if !is_member {
        return Err(AppError::NotFound);
    }

    let server = db::get_server(&state.pool, server_id)
        .await?
        .ok_or(AppError::NotFound)?;

    Ok(Json(server))
}

async fn update_server(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(server_id): Path<Uuid>,
    Json(req): Json<UpdateServerRequest>,
) -> Result<Json<Server>, AppError> {
    let trimmed_name = req.name.as_deref().map(str::trim);
    if let Some(name) = trimmed_name {
        validate_server_name(name)?;
    }
    if let Some(Some(ref url)) = req.icon_url {
        validate_icon_url(url)?;
    }

    let icon_url_ref = req.icon_url.as_ref().map(|o| o.as_deref());

    let server = db::update_server_if_admin(&state.pool, server_id, auth.user_id, trimmed_name, icon_url_ref)
        .await?;

    match server {
        Some(s) => Ok(Json(s)),
        None => {
            let exists = db::get_server(&state.pool, server_id).await?.is_some();
            if exists {
                Err(AppError::Forbidden)
            } else {
                Err(AppError::NotFound)
            }
        }
    }
}

async fn delete_server(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(server_id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let deleted = db::delete_server_if_owner(&state.pool, server_id, auth.user_id).await?;

    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        let exists = db::get_server(&state.pool, server_id).await?.is_some();
        if exists {
            Err(AppError::Forbidden)
        } else {
            Err(AppError::NotFound)
        }
    }
}
