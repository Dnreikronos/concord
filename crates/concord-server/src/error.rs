use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use concord_shared::validation::ValidationError;

#[derive(Debug)]
pub enum AppError {
    Validation(ValidationError),
    UsernameExists,
    EmailExists,
    InvalidCredentials,
    Unauthorized,
    InvalidToken,
    OAuthIdentityExists,
    OAuthNotConfigured,
    OAuthFailed(String),
    AlreadyMember,
    InvalidInviteCode,
    NotFound,
    Forbidden,
    Internal(String),
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            Self::Validation(e) => (StatusCode::BAD_REQUEST, e.to_string()),
            Self::UsernameExists => {
                (StatusCode::CONFLICT, "username already exists".into())
            }
            Self::EmailExists => {
                (StatusCode::CONFLICT, "email already exists".into())
            }
            Self::InvalidCredentials => {
                (StatusCode::UNAUTHORIZED, "invalid email or password".into())
            }
            Self::Unauthorized => {
                (StatusCode::UNAUTHORIZED, "missing or invalid authorization".into())
            }
            Self::InvalidToken => {
                (StatusCode::UNAUTHORIZED, "invalid or expired token".into())
            }
            Self::OAuthIdentityExists => {
                (StatusCode::CONFLICT, "OAuth identity already linked".into())
            }
            Self::OAuthNotConfigured => {
                (StatusCode::NOT_FOUND, "OAuth provider is not configured".into())
            }
            Self::OAuthFailed(msg) => {
                eprintln!("oauth error: {msg}");
                (StatusCode::BAD_GATEWAY, "OAuth authentication failed".into())
            }
            Self::AlreadyMember => {
                (StatusCode::CONFLICT, "already a member of this server".into())
            }
            Self::InvalidInviteCode => {
                (StatusCode::BAD_REQUEST, "invalid or expired invite code".into())
            }
            Self::NotFound => {
                (StatusCode::NOT_FOUND, "not found".into())
            }
            Self::Forbidden => {
                (StatusCode::FORBIDDEN, "forbidden".into())
            }
            Self::Internal(msg) => {
                eprintln!("internal error: {msg}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal server error".into())
            }
        };
        (status, Json(ErrorBody { error: message })).into_response()
    }
}

impl From<ValidationError> for AppError {
    fn from(e: ValidationError) -> Self {
        Self::Validation(e)
    }
}

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        if let sqlx::Error::Database(ref db_err) = e {
            if db_err.is_unique_violation() {
                let constraint = db_err.constraint().unwrap_or("");
                if constraint.contains("username") {
                    return Self::UsernameExists;
                }
                if constraint.contains("email") {
                    return Self::EmailExists;
                }
                if constraint.contains("oauth_identity") {
                    return Self::OAuthIdentityExists;
                }
                if constraint.contains("server_members_pkey") {
                    return Self::AlreadyMember;
                }
            }
        }
        Self::Internal(e.to_string())
    }
}
