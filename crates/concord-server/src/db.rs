use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use concord_shared::types::User;

use crate::error::AppError;

pub async fn insert_user(
    pool: &PgPool,
    username: &str,
    email: &str,
    password_hash: &str,
) -> Result<User, AppError> {
    let row = sqlx::query_as::<_, UserRow>(
        "INSERT INTO users (username, email, password_hash) \
         VALUES ($1, $2, $3) \
         RETURNING id, username, email, password_hash, avatar_url, \
                   status, oauth_provider, oauth_subject, \
                   created_at, updated_at",
    )
    .bind(username)
    .bind(email)
    .bind(password_hash)
    .fetch_one(pool)
    .await?;

    row.into_user()
}

#[derive(sqlx::FromRow)]
struct UserRow {
    id: Uuid,
    username: String,
    email: Option<String>,
    password_hash: Option<String>,
    avatar_url: Option<String>,
    status: String,
    oauth_provider: Option<String>,
    oauth_subject: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl UserRow {
    fn into_user(self) -> Result<User, AppError> {
        let status = self
            .status
            .parse()
            .map_err(|e: String| AppError::Internal(e))?;

        let oauth_provider = self
            .oauth_provider
            .map(|s| s.parse())
            .transpose()
            .map_err(|e: String| AppError::Internal(e))?;

        Ok(User {
            id: self.id,
            username: self.username,
            email: self.email,
            password_hash: self.password_hash,
            avatar_url: self.avatar_url,
            status,
            oauth_provider,
            oauth_subject: self.oauth_subject,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}
