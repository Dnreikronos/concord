use std::sync::Arc;

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use chrono::Utc;
use sha2::{Digest, Sha256};

use concord_shared::types::User;
use concord_shared::validation::{validate_email, validate_password, validate_username};

use secrecy::ExposeSecret;

use crate::db;
use crate::error::AppError;
use crate::jwt;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct RegisterRequest {
    pub username: String,
    pub email: String,
    pub password: String,
}

#[derive(Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

#[derive(Serialize)]
pub struct LoginResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub user: User,
}

#[derive(Serialize)]
pub struct RefreshResponse {
    pub access_token: String,
    pub refresh_token: String,
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/register", post(register))
        .route("/login", post(login))
        .route("/refresh", post(refresh))
}

async fn register(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RegisterRequest>,
) -> Result<(StatusCode, Json<User>), AppError> {
    let username = req.username.trim();
    let email = req.email.trim();

    validate_username(username)?;
    validate_email(email)?;
    validate_password(&req.password)?;

    let salt = SaltString::generate(&mut OsRng);
    let password_hash = Argon2::default()
        .hash_password(req.password.as_bytes(), &salt)
        .map_err(|e| AppError::Internal(e.to_string()))?
        .to_string();

    let user =
        db::insert_user(&state.pool, username, email, &password_hash).await?;

    Ok((StatusCode::CREATED, Json(user)))
}

async fn login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, AppError> {
    let email = req.email.trim();

    let user = db::get_user_by_email(&state.pool, email)
        .await?
        .ok_or(AppError::InvalidCredentials)?;

    let stored_hash = user
        .password_hash
        .as_deref()
        .ok_or(AppError::InvalidCredentials)?;

    let parsed_hash = PasswordHash::new(stored_hash)
        .map_err(|e| AppError::Internal(e.to_string()))?;

    Argon2::default()
        .verify_password(req.password.as_bytes(), &parsed_hash)
        .map_err(|_| AppError::InvalidCredentials)?;

    let access_token = jwt::encode_access_token(user.id, state.jwt_secret.expose_secret())
        .map_err(|e| AppError::Internal(e.to_string()))?;

    let refresh = jwt::generate_refresh_token();
    db::insert_refresh_token(&state.pool, user.id, &refresh.hash, refresh.expires_at)
        .await?;

    Ok(Json(LoginResponse {
        access_token,
        refresh_token: refresh.raw,
        user,
    }))
}

async fn refresh(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RefreshRequest>,
) -> Result<Json<RefreshResponse>, AppError> {
    let old_hash = hex::encode(Sha256::digest(req.refresh_token.as_bytes()));

    let row = db::take_refresh_token(&state.pool, &old_hash)
        .await?
        .ok_or(AppError::InvalidToken)?;

    if row.expires_at < Utc::now() {
        return Err(AppError::InvalidToken);
    }

    let access_token =
        jwt::encode_access_token(row.user_id, state.jwt_secret.expose_secret())
            .map_err(|e| AppError::Internal(e.to_string()))?;

    let new_refresh = jwt::generate_refresh_token();
    db::insert_refresh_token(
        &state.pool,
        row.user_id,
        &new_refresh.hash,
        new_refresh.expires_at,
    )
    .await?;

    Ok(Json(RefreshResponse {
        access_token,
        refresh_token: new_refresh.raw,
    }))
}
