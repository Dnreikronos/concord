use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use rand::Rng as _;
use serde::Deserialize;
use uuid::Uuid;

use concord_shared::types::{MemberInfo, Server, ServerInvite};
use concord_shared::validation::{validate_icon_url, validate_invite_code, validate_server_name};

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
        .route("/{id}/invites", post(create_invite))
        .route("/{id}/join", post(join_server))
        .route("/{id}/members", get(list_members))
        .route("/{id}/members/@me", delete(leave_server))
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
    let server = db::get_server_for_member(&state.pool, server_id, auth.user_id)
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
    if req.name.is_none() && req.icon_url.is_none() {
        return Err(AppError::Validation(
            concord_shared::validation::ValidationError::BlankContent { field: "request body" },
        ));
    }

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

    server.map(Json).ok_or(AppError::NotFound)
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
        Err(AppError::NotFound)
    }
}

#[derive(Deserialize)]
struct CreateInviteRequest {
    #[serde(default)]
    max_uses: Option<i32>,
    #[serde(default)]
    expires_at: Option<DateTime<Utc>>,
}

fn generate_invite_code() -> String {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..8).map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char).collect()
}

async fn create_invite(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(server_id): Path<Uuid>,
    Json(req): Json<CreateInviteRequest>,
) -> Result<(StatusCode, Json<ServerInvite>), AppError> {
    if !db::is_server_admin(&state.pool, server_id, auth.user_id).await? {
        if !db::server_exists(&state.pool, server_id).await? {
            return Err(AppError::NotFound);
        }
        return Err(AppError::Forbidden);
    }

    if let Some(n) = req.max_uses {
        if n <= 0 {
            return Err(AppError::Validation(
                concord_shared::validation::ValidationError::InvalidValue {
                    field: "max_uses",
                    reason: "must be positive",
                },
            ));
        }
    }
    if let Some(exp) = req.expires_at {
        if exp <= Utc::now() {
            return Err(AppError::Validation(
                concord_shared::validation::ValidationError::InvalidValue {
                    field: "expires_at",
                    reason: "must be in the future",
                },
            ));
        }
    }

    let invite = {
        let mut attempts = 0;
        loop {
            let code = generate_invite_code();
            match db::create_invite(
                &state.pool,
                server_id,
                auth.user_id,
                &code,
                req.max_uses,
                req.expires_at,
            )
            .await
            {
                Ok(inv) => break inv,
                Err(AppError::InviteCodeCollision) if attempts < 3 => {
                    attempts += 1;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    };

    Ok((StatusCode::CREATED, Json(invite)))
}

#[derive(Deserialize)]
struct JoinServerRequest {
    invite_code: String,
}

async fn join_server(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(server_id): Path<Uuid>,
    Json(req): Json<JoinServerRequest>,
) -> Result<StatusCode, AppError> {
    validate_invite_code(&req.invite_code)?;

    if !db::server_exists(&state.pool, server_id).await? {
        return Err(AppError::NotFound);
    }

    let mut tx = state.pool.begin().await.map_err(|e| AppError::Internal(e.to_string()))?;

    db::claim_invite(&mut *tx, server_id, &req.invite_code)
        .await?
        .ok_or(AppError::InvalidInviteCode)?;

    db::insert_server_member(&mut *tx, server_id, auth.user_id, "member").await?;

    tx.commit().await.map_err(|e| AppError::Internal(e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

async fn leave_server(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(server_id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    if !db::is_server_member(&state.pool, server_id, auth.user_id).await? {
        return Err(AppError::NotFound);
    }

    if db::is_server_owner(&state.pool, server_id, auth.user_id).await? {
        return Err(AppError::Forbidden);
    }

    db::remove_server_member(&state.pool, server_id, auth.user_id).await?;

    Ok(StatusCode::NO_CONTENT)
}

async fn list_members(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(server_id): Path<Uuid>,
) -> Result<Json<Vec<MemberInfo>>, AppError> {
    if !db::is_server_member(&state.pool, server_id, auth.user_id).await? {
        if !db::server_exists(&state.pool, server_id).await? {
            return Err(AppError::NotFound);
        }
        return Err(AppError::NotFound);
    }

    let members = db::list_server_members(&state.pool, server_id).await?;
    Ok(Json(members))
}
