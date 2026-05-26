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

pub async fn get_message_author(
    pool: &PgPool,
    message_id: Uuid,
) -> Result<Option<Uuid>, AppError> {
    let row = sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT author_id FROM messages WHERE id = $1",
    )
    .bind(message_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.flatten())
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

pub async fn update_message_content(
    pool: &PgPool,
    message_id: Uuid,
    content: &str,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE messages SET content = $2, edited_at = now() WHERE id = $1",
    )
    .bind(message_id)
    .bind(content)
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn delete_message(pool: &PgPool, message_id: Uuid) -> Result<(), AppError> {
    sqlx::query("DELETE FROM messages WHERE id = $1")
        .bind(message_id)
        .execute(pool)
        .await?;

    Ok(())
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

pub async fn is_server_admin(
    pool: &PgPool,
    user_id: Uuid,
    server_id: Uuid,
) -> Result<bool, AppError> {
    let is_owner = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM servers WHERE id = $1 AND owner_id = $2)",
    )
    .bind(server_id)
    .bind(user_id)
    .fetch_one(pool)
    .await?;

    if is_owner {
        return Ok(true);
    }

    let role = sqlx::query_scalar::<_, String>(
        "SELECT role FROM server_members WHERE server_id = $1 AND user_id = $2",
    )
    .bind(server_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;

    Ok(role.as_deref() == Some("admin"))
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
