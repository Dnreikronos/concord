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

pub async fn get_user_by_email(
    pool: &PgPool,
    email: &str,
) -> Result<Option<User>, AppError> {
    let row = sqlx::query_as::<_, UserRow>(
        "SELECT id, username, email, password_hash, avatar_url, \
                status, oauth_provider, oauth_subject, \
                created_at, updated_at \
         FROM users WHERE email = $1",
    )
    .bind(email)
    .fetch_optional(pool)
    .await?;

    row.map(|r| r.into_user()).transpose()
}

pub async fn insert_refresh_token(
    pool: &PgPool,
    user_id: Uuid,
    token_hash: &str,
    expires_at: DateTime<Utc>,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO refresh_tokens (user_id, token_hash, expires_at) \
         VALUES ($1, $2, $3)",
    )
    .bind(user_id)
    .bind(token_hash)
    .bind(expires_at)
    .execute(pool)
    .await?;

    Ok(())
}

#[derive(sqlx::FromRow)]
pub struct RefreshTokenRow {
    pub user_id: Uuid,
    pub expires_at: DateTime<Utc>,
}

pub async fn get_refresh_token(
    pool: &PgPool,
    token_hash: &str,
) -> Result<Option<RefreshTokenRow>, AppError> {
    let row = sqlx::query_as::<_, RefreshTokenRow>(
        "SELECT user_id, expires_at FROM refresh_tokens WHERE token_hash = $1",
    )
    .bind(token_hash)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

pub async fn delete_refresh_token(
    pool: &PgPool,
    token_hash: &str,
) -> Result<(), AppError> {
    sqlx::query("DELETE FROM refresh_tokens WHERE token_hash = $1")
        .bind(token_hash)
        .execute(pool)
        .await?;

    Ok(())
}

pub async fn get_message_channel(
    pool: &PgPool,
    message_id: Uuid,
) -> Result<Option<Uuid>, AppError> {
    let row =
        sqlx::query_scalar::<_, Uuid>("SELECT channel_id FROM messages WHERE id = $1")
            .bind(message_id)
            .fetch_optional(pool)
            .await?;

    Ok(row)
}

/// Atomically update content only if author_id matches. Returns the
/// channel_id on success, None if the message doesn't exist or the
/// caller isn't the author.
pub async fn update_message_if_author(
    pool: &PgPool,
    message_id: Uuid,
    author_id: Uuid,
    content: &str,
) -> Result<Option<Uuid>, AppError> {
    let row = sqlx::query_scalar::<_, Uuid>(
        "UPDATE messages SET content = $2, edited_at = now() \
         WHERE id = $1 AND author_id = $3 \
         RETURNING channel_id",
    )
    .bind(message_id)
    .bind(content)
    .bind(author_id)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

/// Delete message only if author matches. Returns channel_id on success.
pub async fn delete_message_if_author(
    pool: &PgPool,
    message_id: Uuid,
    author_id: Uuid,
) -> Result<Option<Uuid>, AppError> {
    let row = sqlx::query_scalar::<_, Uuid>(
        "DELETE FROM messages WHERE id = $1 AND author_id = $2 \
         RETURNING channel_id",
    )
    .bind(message_id)
    .bind(author_id)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

/// Delete message unconditionally (admin path). Returns channel_id on success.
pub async fn delete_message(
    pool: &PgPool,
    message_id: Uuid,
) -> Result<Option<Uuid>, AppError> {
    let row = sqlx::query_scalar::<_, Uuid>(
        "DELETE FROM messages WHERE id = $1 RETURNING channel_id",
    )
    .bind(message_id)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

pub async fn is_server_admin(
    pool: &PgPool,
    user_id: Uuid,
    server_id: Uuid,
) -> Result<bool, AppError> {
    let result = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(\
             SELECT 1 FROM servers WHERE id = $1 AND owner_id = $2 \
             UNION ALL \
             SELECT 1 FROM server_members \
             WHERE server_id = $1 AND user_id = $2 AND role = 'admin'\
         )",
    )
    .bind(server_id)
    .bind(user_id)
    .fetch_one(pool)
    .await?;

    Ok(result)
}

pub async fn get_channel_server(
    pool: &PgPool,
    channel_id: Uuid,
) -> Result<Option<Uuid>, AppError> {
    let row =
        sqlx::query_scalar::<_, Uuid>("SELECT server_id FROM channels WHERE id = $1")
            .bind(channel_id)
            .fetch_optional(pool)
            .await?;

    Ok(row)
}

pub async fn get_user_by_oauth(
    pool: &PgPool,
    provider: &str,
    subject: &str,
) -> Result<Option<User>, AppError> {
    let row = sqlx::query_as::<_, UserRow>(
        "SELECT id, username, email, password_hash, avatar_url, \
                status, oauth_provider, oauth_subject, \
                created_at, updated_at \
         FROM users WHERE oauth_provider = $1 AND oauth_subject = $2",
    )
    .bind(provider)
    .bind(subject)
    .fetch_optional(pool)
    .await?;

    row.map(|r| r.into_user()).transpose()
}

pub async fn insert_oauth_user(
    pool: &PgPool,
    username: &str,
    email: Option<&str>,
    avatar_url: Option<&str>,
    oauth_provider: &str,
    oauth_subject: &str,
) -> Result<User, AppError> {
    let row = sqlx::query_as::<_, UserRow>(
        "INSERT INTO users (username, email, avatar_url, oauth_provider, oauth_subject) \
         VALUES ($1, $2, $3, $4, $5) \
         RETURNING id, username, email, password_hash, avatar_url, \
                   status, oauth_provider, oauth_subject, \
                   created_at, updated_at",
    )
    .bind(username)
    .bind(email)
    .bind(avatar_url)
    .bind(oauth_provider)
    .bind(oauth_subject)
    .fetch_one(pool)
    .await?;

    row.into_user()
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
