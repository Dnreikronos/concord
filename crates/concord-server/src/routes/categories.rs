use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::patch;
use axum::{Json, Router};
use serde::Deserialize;
use uuid::Uuid;

use concord_shared::types::ChannelCategory;
use concord_shared::validation::validate_category_name;

use crate::db;
use crate::error::AppError;
use crate::extractors::AuthUser;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct CreateCategoryRequest {
    name: String,
}

#[derive(Deserialize)]
struct RenameCategoryRequest {
    name: String,
}

#[derive(Deserialize)]
pub struct ReorderRequest {
    #[serde(default)]
    channels: Vec<ReorderChannel>,
    #[serde(default)]
    categories: Vec<ReorderCategory>,
}

#[derive(Deserialize)]
pub struct ReorderChannel {
    id: Uuid,
    category_id: Option<Uuid>,
    position: i32,
}

#[derive(Deserialize)]
pub struct ReorderCategory {
    id: Uuid,
    position: i32,
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/{id}", patch(rename_category).delete(delete_category))
}

/// `GET /api/servers/{id}/categories` — the channel categories of `server_id`,
/// ordered by position. Any member may read them; the sidebar groups channels
/// under these.
pub async fn list_categories(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(server_id): Path<Uuid>,
) -> Result<Json<Vec<ChannelCategory>>, AppError> {
    if !db::is_server_member(&state.pool, server_id, auth.user_id).await? {
        if !db::server_exists(&state.pool, server_id).await? {
            return Err(AppError::NotFound);
        }
        return Err(AppError::Forbidden);
    }

    let categories = db::list_categories_for_server(&state.pool, server_id).await?;
    Ok(Json(categories))
}

pub async fn create_category(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(server_id): Path<Uuid>,
    Json(req): Json<CreateCategoryRequest>,
) -> Result<(StatusCode, Json<ChannelCategory>), AppError> {
    if !db::is_server_admin(&state.pool, server_id, auth.user_id).await? {
        if !db::server_exists(&state.pool, server_id).await? {
            return Err(AppError::NotFound);
        }
        return Err(AppError::Forbidden);
    }

    let name = req.name.trim();
    validate_category_name(name)?;

    let category = db::insert_category(&state.pool, server_id, name).await?;
    Ok((StatusCode::CREATED, Json(category)))
}

async fn rename_category(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(category_id): Path<Uuid>,
    Json(req): Json<RenameCategoryRequest>,
) -> Result<Json<ChannelCategory>, AppError> {
    let name = req.name.trim();
    validate_category_name(name)?;

    let server_id = db::get_category_server_id(&state.pool, category_id)
        .await?
        .ok_or(AppError::NotFound)?;

    if !db::is_server_admin(&state.pool, server_id, auth.user_id).await? {
        return Err(AppError::Forbidden);
    }

    let category =
        db::rename_category_if_admin(&state.pool, category_id, auth.user_id, name).await?;

    category.map(Json).ok_or(AppError::NotFound)
}

async fn delete_category(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(category_id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let server_id = db::get_category_server_id(&state.pool, category_id)
        .await?
        .ok_or(AppError::NotFound)?;

    if !db::is_server_admin(&state.pool, server_id, auth.user_id).await? {
        return Err(AppError::Forbidden);
    }

    let deleted =
        db::delete_category_if_admin(&state.pool, category_id, auth.user_id).await?;

    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

pub async fn reorder(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(server_id): Path<Uuid>,
    Json(req): Json<ReorderRequest>,
) -> Result<StatusCode, AppError> {
    if req.channels.is_empty() && req.categories.is_empty() {
        return Err(AppError::Validation(
            concord_shared::validation::ValidationError::BlankContent {
                field: "request body",
            },
        ));
    }

    const MAX_REORDER_ITEMS: usize = 500;
    if req.channels.len() > MAX_REORDER_ITEMS || req.categories.len() > MAX_REORDER_ITEMS {
        return Err(AppError::Validation(
            concord_shared::validation::ValidationError::InvalidValue {
                field: "reorder",
                reason: "too many items in a single request",
            },
        ));
    }

    if !db::is_server_admin(&state.pool, server_id, auth.user_id).await? {
        if !db::server_exists(&state.pool, server_id).await? {
            return Err(AppError::NotFound);
        }
        return Err(AppError::Forbidden);
    }

    let channels: Vec<(Uuid, Option<Uuid>, i32)> = req
        .channels
        .iter()
        .map(|c| (c.id, c.category_id, c.position))
        .collect();

    let categories: Vec<(Uuid, i32)> =
        req.categories.iter().map(|c| (c.id, c.position)).collect();

    db::reorder_channels(&state.pool, server_id, &channels, &categories).await?;

    Ok(StatusCode::NO_CONTENT)
}
