use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use secrecy::ExposeSecret;
use uuid::Uuid;

use crate::error::AppError;
use crate::state::AppState;

pub struct AuthUser {
    pub user_id: Uuid,
}

impl FromRequestParts<Arc<AppState>> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .ok_or(AppError::Unauthorized)?;

        let token = header
            .strip_prefix("Bearer ")
            .ok_or(AppError::Unauthorized)?;

        let claims =
            crate::jwt::decode_access_token(token, state.jwt_secret.expose_secret())
                .map_err(|_| AppError::InvalidToken)?;

        Ok(AuthUser {
            user_id: claims.sub,
        })
    }
}
