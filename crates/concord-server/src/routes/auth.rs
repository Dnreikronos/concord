use std::sync::Arc;

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHasher};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;

use concord_shared::types::User;
use concord_shared::validation::{validate_email, validate_password, validate_username};

use crate::db;
use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct RegisterRequest {
    pub username: String,
    pub email: String,
    pub password: String,
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/register", post(register))
}

async fn register(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RegisterRequest>,
) -> Result<(StatusCode, Json<User>), AppError> {
    validate_username(&req.username)?;
    validate_email(&req.email)?;
    validate_password(&req.password)?;

    let salt = SaltString::generate(&mut OsRng);
    let password_hash = Argon2::default()
        .hash_password(req.password.as_bytes(), &salt)
        .map_err(|e| AppError::Internal(e.to_string()))?
        .to_string();

    let user =
        db::insert_user(&state.pool, &req.username, &req.email, &password_hash).await?;

    Ok((StatusCode::CREATED, Json(user)))
}
