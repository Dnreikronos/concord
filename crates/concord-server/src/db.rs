use chrono::{DateTime, Utc};
use sqlx::{Executor, PgPool, Postgres};
use uuid::Uuid;

use concord_shared::types::{Channel, Server, User};

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

pub async fn take_refresh_token(
    pool: &PgPool,
    token_hash: &str,
) -> Result<Option<RefreshTokenRow>, AppError> {
    let row = sqlx::query_as::<_, RefreshTokenRow>(
        "DELETE FROM refresh_tokens WHERE token_hash = $1 \
         RETURNING user_id, expires_at",
    )
    .bind(token_hash)
    .fetch_optional(pool)
    .await?;

    Ok(row)
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
    server_id: Uuid,
    user_id: Uuid,
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

#[derive(sqlx::FromRow)]
struct ServerRow {
    id: Uuid,
    name: String,
    icon_url: Option<String>,
    owner_id: Uuid,
    created_at: DateTime<Utc>,
}

impl ServerRow {
    fn into_server(self) -> Server {
        Server {
            id: self.id,
            name: self.name,
            icon_url: self.icon_url,
            owner_id: self.owner_id,
            created_at: self.created_at,
        }
    }
}

#[derive(sqlx::FromRow)]
struct ChannelRow {
    id: Uuid,
    server_id: Uuid,
    category_id: Option<Uuid>,
    name: String,
    topic: Option<String>,
    channel_type: String,
    position: i32,
    created_at: DateTime<Utc>,
}

impl ChannelRow {
    fn into_channel(self) -> Result<Channel, AppError> {
        let channel_type = match self.channel_type.as_str() {
            "text" => concord_shared::types::ChannelType::Text,
            "voice" => concord_shared::types::ChannelType::Voice,
            other => return Err(AppError::Internal(format!("unknown channel_type: {other}"))),
        };
        Ok(Channel {
            id: self.id,
            server_id: self.server_id,
            category_id: self.category_id,
            name: self.name,
            topic: self.topic,
            channel_type,
            position: self.position,
            created_at: self.created_at,
        })
    }
}

pub async fn insert_server<'e, E>(
    executor: E,
    name: &str,
    icon_url: Option<&str>,
    owner_id: Uuid,
) -> Result<Server, AppError>
where
    E: Executor<'e, Database = Postgres>,
{
    let row = sqlx::query_as::<_, ServerRow>(
        "INSERT INTO servers (name, icon_url, owner_id) \
         VALUES ($1, $2, $3) \
         RETURNING id, name, icon_url, owner_id, created_at",
    )
    .bind(name)
    .bind(icon_url)
    .bind(owner_id)
    .fetch_one(executor)
    .await?;

    Ok(row.into_server())
}

pub async fn insert_server_member<'e, E>(
    executor: E,
    server_id: Uuid,
    user_id: Uuid,
    role: &str,
) -> Result<(), AppError>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query(
        "INSERT INTO server_members (server_id, user_id, role) \
         VALUES ($1, $2, $3)",
    )
    .bind(server_id)
    .bind(user_id)
    .bind(role)
    .execute(executor)
    .await?;

    Ok(())
}

pub async fn insert_channel<'e, E>(
    executor: E,
    server_id: Uuid,
    name: &str,
    channel_type: &str,
    position: i32,
) -> Result<Channel, AppError>
where
    E: Executor<'e, Database = Postgres>,
{
    let row = sqlx::query_as::<_, ChannelRow>(
        "INSERT INTO channels (server_id, name, channel_type, position) \
         VALUES ($1, $2, $3, $4) \
         RETURNING id, server_id, category_id, name, topic, \
                   channel_type, position, created_at",
    )
    .bind(server_id)
    .bind(name)
    .bind(channel_type)
    .bind(position)
    .fetch_one(executor)
    .await?;

    row.into_channel()
}

pub async fn list_servers_for_user(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Vec<Server>, AppError> {
    let rows = sqlx::query_as::<_, ServerRow>(
        "SELECT s.id, s.name, s.icon_url, s.owner_id, s.created_at \
         FROM servers s \
         JOIN server_members sm ON sm.server_id = s.id \
         WHERE sm.user_id = $1 \
         ORDER BY s.created_at",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(ServerRow::into_server).collect())
}

pub async fn get_server(
    pool: &PgPool,
    server_id: Uuid,
) -> Result<Option<Server>, AppError> {
    let row = sqlx::query_as::<_, ServerRow>(
        "SELECT id, name, icon_url, owner_id, created_at \
         FROM servers WHERE id = $1",
    )
    .bind(server_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(ServerRow::into_server))
}

pub async fn get_server_for_member(
    pool: &PgPool,
    server_id: Uuid,
    user_id: Uuid,
) -> Result<Option<Server>, AppError> {
    let row = sqlx::query_as::<_, ServerRow>(
        "SELECT s.id, s.name, s.icon_url, s.owner_id, s.created_at \
         FROM servers s \
         JOIN server_members sm ON sm.server_id = s.id \
         WHERE s.id = $1 AND sm.user_id = $2",
    )
    .bind(server_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(ServerRow::into_server))
}

pub async fn update_server_if_admin(
    pool: &PgPool,
    server_id: Uuid,
    user_id: Uuid,
    name: Option<&str>,
    icon_url: Option<Option<&str>>,
) -> Result<Option<Server>, AppError> {
    let row = sqlx::query_as::<_, ServerRow>(
        "UPDATE servers SET \
             name = COALESCE($3, name), \
             icon_url = CASE WHEN $4 THEN $5 ELSE icon_url END \
         WHERE id = $1 \
           AND (owner_id = $2 OR EXISTS(\
                SELECT 1 FROM server_members \
                WHERE server_id = $1 AND user_id = $2 AND role = 'admin'\
           )) \
         RETURNING id, name, icon_url, owner_id, created_at",
    )
    .bind(server_id)
    .bind(user_id)
    .bind(name)
    .bind(icon_url.is_some())
    .bind(icon_url.flatten())
    .fetch_optional(pool)
    .await?;

    Ok(row.map(ServerRow::into_server))
}

pub async fn delete_server_if_owner(
    pool: &PgPool,
    server_id: Uuid,
    owner_id: Uuid,
) -> Result<bool, AppError> {
    let result = sqlx::query("DELETE FROM servers WHERE id = $1 AND owner_id = $2")
        .bind(server_id)
        .bind(owner_id)
        .execute(pool)
        .await?;

    Ok(result.rows_affected() > 0)
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
