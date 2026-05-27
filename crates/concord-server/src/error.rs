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
            }
        }
        Self::Internal(e.to_string())
    }
}
