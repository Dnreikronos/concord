use chrono::{DateTime, Utc};
use sqlx::{Executor, PgPool, Postgres};
use uuid::Uuid;

use concord_shared::types::{
    Channel, DmChannel, DmChannelInfo, DmConversation, DmLastMessage, DmParticipant, FriendRequest,
    FriendRequestDirection, FriendRequests, MemberInfo, MessageAuthor, MessageWithAuthor, Server,
    ServerInvite, User, UserSummary,
};

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

#[derive(sqlx::FromRow)]
pub struct InsertedMessage {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
}

pub async fn insert_message(
    pool: &PgPool,
    channel_id: Uuid,
    author_id: Uuid,
    content: &str,
) -> Result<InsertedMessage, AppError> {
    let row = sqlx::query_as::<_, InsertedMessage>(
        "INSERT INTO messages (channel_id, author_id, content) \
         VALUES ($1, $2, $3) \
         RETURNING id, created_at",
    )
    .bind(channel_id)
    .bind(author_id)
    .bind(content)
    .fetch_one(pool)
    .await?;

    Ok(row)
}

/// Whether a user may read a channel's message history.
///
/// A path `{id}` may name either a server channel (`channels`) or a DM channel
/// (`dm_channels`). Both id spaces are disjoint random UUIDs, so a single
/// lookup resolves which kind it is and whether the caller belongs to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelAccess {
    /// The caller is a member and may read the channel.
    Authorized,
    /// The channel exists but the caller is not a member.
    Forbidden,
    /// No channel — server or DM — has this id.
    NotFound,
}

#[derive(sqlx::FromRow)]
struct ChannelAccessRow {
    channel_exists: bool,
    is_member: bool,
}

pub async fn check_channel_read_access(
    pool: &PgPool,
    channel_id: Uuid,
    user_id: Uuid,
) -> Result<ChannelAccess, AppError> {
    let row = sqlx::query_as::<_, ChannelAccessRow>(
        "SELECT \
             (EXISTS(SELECT 1 FROM channels WHERE id = $1) \
                OR EXISTS(SELECT 1 FROM dm_channels WHERE id = $1)) AS channel_exists, \
             EXISTS(\
                 SELECT 1 FROM channels c \
                 JOIN server_members sm ON sm.server_id = c.server_id \
                 WHERE c.id = $1 AND sm.user_id = $2 \
                 UNION ALL \
                 SELECT 1 FROM dm_members \
                 WHERE dm_channel_id = $1 AND user_id = $2\
             ) AS is_member",
    )
    .bind(channel_id)
    .bind(user_id)
    .fetch_one(pool)
    .await?;

    Ok(if row.is_member {
        ChannelAccess::Authorized
    } else if row.channel_exists {
        ChannelAccess::Forbidden
    } else {
        ChannelAccess::NotFound
    })
}

#[derive(sqlx::FromRow)]
struct MessageWithAuthorRow {
    id: Uuid,
    channel_id: Uuid,
    author_id: Option<Uuid>,
    content: String,
    edited_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    author_username: Option<String>,
    author_avatar_url: Option<String>,
}

impl MessageWithAuthorRow {
    fn into_message(self) -> MessageWithAuthor {
        // author_id is NULL once the account is deleted (ON DELETE SET NULL);
        // the LEFT JOIN then yields no username either. Both columns travel
        // together, so matching on the pair keeps the author whole-or-absent.
        let author = match (self.author_id, self.author_username) {
            (Some(id), Some(username)) => Some(MessageAuthor {
                id,
                username,
                avatar_url: self.author_avatar_url,
            }),
            _ => None,
        };

        MessageWithAuthor {
            id: self.id,
            channel_id: self.channel_id,
            author,
            content: self.content,
            edited_at: self.edited_at,
            created_at: self.created_at,
        }
    }
}

/// Fetch up to `limit` messages from a channel, newest first, paginating
/// backwards from (but excluding) the `before` message when given.
///
/// The cursor compares the `(created_at, id)` tuple rather than `created_at`
/// alone: message ids are random UUIDv4, not time-ordered, so two messages can
/// share a `created_at`. The tuple gives a total order that matches the
/// `ORDER BY` exactly, so pages never drop or repeat a row at a timestamp
/// boundary. A `before` id that isn't in this channel yields no rows.
pub async fn list_channel_messages(
    pool: &PgPool,
    channel_id: Uuid,
    before: Option<Uuid>,
    limit: i64,
) -> Result<Vec<MessageWithAuthor>, AppError> {
    let rows = sqlx::query_as::<_, MessageWithAuthorRow>(
        "SELECT m.id, m.channel_id, m.author_id, m.content, \
                m.edited_at, m.created_at, \
                u.username AS author_username, \
                u.avatar_url AS author_avatar_url \
         FROM messages m \
         LEFT JOIN users u ON u.id = m.author_id \
         WHERE m.channel_id = $1 \
           AND ($2::uuid IS NULL OR (m.created_at, m.id) < (\
                   SELECT created_at, id FROM messages \
                   WHERE id = $2 AND channel_id = $1\
               )) \
         ORDER BY m.created_at DESC, m.id DESC \
         LIMIT $3",
    )
    .bind(channel_id)
    .bind(before)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(MessageWithAuthorRow::into_message)
        .collect())
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
/// `(channel_id, edited_at)` on success, None if the message doesn't exist or
/// the caller isn't the author.
pub async fn update_message_if_author(
    pool: &PgPool,
    message_id: Uuid,
    author_id: Uuid,
    content: &str,
) -> Result<Option<(Uuid, DateTime<Utc>)>, AppError> {
    let row = sqlx::query_as::<_, (Uuid, DateTime<Utc>)>(
        "UPDATE messages SET content = $2, edited_at = now() \
         WHERE id = $1 AND author_id = $3 \
         RETURNING channel_id, edited_at",
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
    topic: Option<&str>,
    channel_type: &str,
) -> Result<Channel, AppError>
where
    E: Executor<'e, Database = Postgres>,
{
    let row = sqlx::query_as::<_, ChannelRow>(
        "INSERT INTO channels (server_id, name, topic, channel_type, position) \
         VALUES ($1, $2, $3, $4, (SELECT COALESCE(MAX(position), -1) + 1 FROM channels WHERE server_id = $1)) \
         RETURNING id, server_id, category_id, name, topic, \
                   channel_type, position, created_at",
    )
    .bind(server_id)
    .bind(name)
    .bind(topic)
    .bind(channel_type)
    .fetch_one(executor)
    .await?;

    row.into_channel()
}

pub async fn list_channels_for_server(
    pool: &PgPool,
    server_id: Uuid,
) -> Result<Vec<Channel>, AppError> {
    let rows = sqlx::query_as::<_, ChannelRow>(
        "SELECT id, server_id, category_id, name, topic, \
                channel_type, position, created_at \
         FROM channels \
         WHERE server_id = $1 \
         ORDER BY category_id NULLS FIRST, position, created_at",
    )
    .bind(server_id)
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(ChannelRow::into_channel).collect()
}

pub async fn list_channel_ids_for_user(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Vec<Uuid>, AppError> {
    let ids = sqlx::query_scalar::<_, Uuid>(
        "SELECT c.id FROM channels c \
         JOIN server_members sm ON sm.server_id = c.server_id \
         WHERE sm.user_id = $1",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    Ok(ids)
}

/// Distinct other users relevant to `user_id`'s presence: the people who should
/// learn when `user_id` comes online, changes status, or goes offline (and whose
/// own status `user_id` should receive). That is everyone who shares at least one
/// server, plus every accepted friend — friends matter even when they share no
/// server. The two sets are `UNION`ed, so a user in both appears once; `user_id`
/// itself is excluded.
pub async fn list_presence_peer_ids(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Vec<Uuid>, AppError> {
    let ids = sqlx::query_scalar::<_, Uuid>(
        "SELECT peer.user_id \
         FROM server_members me \
         JOIN server_members peer ON peer.server_id = me.server_id \
         WHERE me.user_id = $1 AND peer.user_id <> $1 \
         UNION \
         SELECT CASE WHEN requester_id = $1 THEN addressee_id ELSE requester_id END \
         FROM friendships \
         WHERE status = 'accepted' AND (requester_id = $1 OR addressee_id = $1)",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    Ok(ids)
}

pub async fn update_channel_if_admin(
    pool: &PgPool,
    channel_id: Uuid,
    user_id: Uuid,
    name: Option<&str>,
    topic: Option<Option<&str>>,
) -> Result<Option<Channel>, AppError> {
    let row = sqlx::query_as::<_, ChannelRow>(
        "UPDATE channels SET \
             name = COALESCE($3, name), \
             topic = CASE WHEN $4 THEN $5 ELSE topic END \
         WHERE id = $1 \
           AND EXISTS(\
               SELECT 1 FROM servers WHERE id = channels.server_id AND owner_id = $2 \
               UNION ALL \
               SELECT 1 FROM server_members \
               WHERE server_id = channels.server_id AND user_id = $2 AND role = 'admin'\
           ) \
         RETURNING id, server_id, category_id, name, topic, \
                   channel_type, position, created_at",
    )
    .bind(channel_id)
    .bind(user_id)
    .bind(name)
    .bind(topic.is_some())
    .bind(topic.flatten())
    .fetch_optional(pool)
    .await?;

    row.map(ChannelRow::into_channel).transpose()
}

pub async fn delete_channel_if_admin(
    pool: &PgPool,
    channel_id: Uuid,
    user_id: Uuid,
) -> Result<bool, AppError> {
    let result = sqlx::query(
        "DELETE FROM channels \
         WHERE id = $1 \
           AND EXISTS(\
               SELECT 1 FROM servers WHERE id = channels.server_id AND owner_id = $2 \
               UNION ALL \
               SELECT 1 FROM server_members \
               WHERE server_id = channels.server_id AND user_id = $2 AND role = 'admin'\
           )",
    )
    .bind(channel_id)
    .bind(user_id)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
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

#[derive(sqlx::FromRow)]
struct InviteRow {
    id: Uuid,
    server_id: Uuid,
    creator_id: Uuid,
    code: String,
    max_uses: Option<i32>,
    uses: i32,
    expires_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

impl InviteRow {
    fn into_invite(self) -> ServerInvite {
        ServerInvite {
            id: self.id,
            server_id: self.server_id,
            creator_id: self.creator_id,
            code: self.code,
            max_uses: self.max_uses,
            uses: self.uses,
            expires_at: self.expires_at,
            created_at: self.created_at,
        }
    }
}

pub async fn create_invite(
    pool: &PgPool,
    server_id: Uuid,
    creator_id: Uuid,
    code: &str,
    max_uses: Option<i32>,
    expires_at: Option<DateTime<Utc>>,
) -> Result<ServerInvite, AppError> {
    let row = sqlx::query_as::<_, InviteRow>(
        "INSERT INTO server_invites (server_id, creator_id, code, max_uses, expires_at) \
         VALUES ($1, $2, $3, $4, $5) \
         RETURNING id, server_id, creator_id, code, max_uses, uses, expires_at, created_at",
    )
    .bind(server_id)
    .bind(creator_id)
    .bind(code)
    .bind(max_uses)
    .bind(expires_at)
    .fetch_one(pool)
    .await?;

    Ok(row.into_invite())
}

pub async fn claim_invite<'e, E>(
    executor: E,
    server_id: Uuid,
    code: &str,
) -> Result<Option<ServerInvite>, AppError>
where
    E: Executor<'e, Database = Postgres>,
{
    let row = sqlx::query_as::<_, InviteRow>(
        "UPDATE server_invites SET uses = uses + 1 \
         WHERE server_id = $1 AND code = $2 \
           AND (expires_at IS NULL OR expires_at > now()) \
           AND (max_uses IS NULL OR uses < max_uses) \
         RETURNING id, server_id, creator_id, code, max_uses, uses, expires_at, created_at",
    )
    .bind(server_id)
    .bind(code)
    .fetch_optional(executor)
    .await?;

    Ok(row.map(InviteRow::into_invite))
}

pub async fn is_server_member(
    pool: &PgPool,
    server_id: Uuid,
    user_id: Uuid,
) -> Result<bool, AppError> {
    let result = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM server_members WHERE server_id = $1 AND user_id = $2)",
    )
    .bind(server_id)
    .bind(user_id)
    .fetch_one(pool)
    .await?;

    Ok(result)
}

pub async fn remove_server_member(
    pool: &PgPool,
    server_id: Uuid,
    user_id: Uuid,
) -> Result<bool, AppError> {
    let result = sqlx::query("DELETE FROM server_members WHERE server_id = $1 AND user_id = $2")
        .bind(server_id)
        .bind(user_id)
        .execute(pool)
        .await?;

    Ok(result.rows_affected() > 0)
}

pub async fn list_server_members(
    pool: &PgPool,
    server_id: Uuid,
) -> Result<Vec<MemberInfo>, AppError> {
    let rows = sqlx::query_as::<_, MemberInfoRow>(
        "SELECT sm.user_id, u.username, u.avatar_url, \
                CASE WHEN s.owner_id = sm.user_id THEN 'owner' ELSE sm.role END AS role, \
                sm.joined_at \
         FROM server_members sm \
         JOIN users u ON u.id = sm.user_id \
         JOIN servers s ON s.id = sm.server_id \
         WHERE sm.server_id = $1 \
         ORDER BY sm.joined_at",
    )
    .bind(server_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(MemberInfoRow::into_member_info).collect())
}

#[derive(sqlx::FromRow)]
struct MemberInfoRow {
    user_id: Uuid,
    username: String,
    avatar_url: Option<String>,
    role: String,
    joined_at: DateTime<Utc>,
}

impl MemberInfoRow {
    fn into_member_info(self) -> MemberInfo {
        MemberInfo {
            user_id: self.user_id,
            username: self.username,
            avatar_url: self.avatar_url,
            role: self.role,
            joined_at: self.joined_at,
        }
    }
}

pub async fn server_exists(
    pool: &PgPool,
    server_id: Uuid,
) -> Result<bool, AppError> {
    let result = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM servers WHERE id = $1)",
    )
    .bind(server_id)
    .fetch_one(pool)
    .await?;

    Ok(result)
}

pub async fn is_server_owner(
    pool: &PgPool,
    server_id: Uuid,
    user_id: Uuid,
) -> Result<bool, AppError> {
    let result = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM servers WHERE id = $1 AND owner_id = $2)",
    )
    .bind(server_id)
    .bind(user_id)
    .fetch_one(pool)
    .await?;

    Ok(result)
}

// ---------------------------------------------------------------------------
// channel_categories
// ---------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct CategoryRow {
    id: Uuid,
    server_id: Uuid,
    name: String,
    position: i32,
    created_at: DateTime<Utc>,
}

impl CategoryRow {
    fn into_category(self) -> concord_shared::types::ChannelCategory {
        concord_shared::types::ChannelCategory {
            id: self.id,
            server_id: self.server_id,
            name: self.name,
            position: self.position,
            created_at: self.created_at,
        }
    }
}

pub async fn list_categories_for_server(
    pool: &PgPool,
    server_id: Uuid,
) -> Result<Vec<concord_shared::types::ChannelCategory>, AppError> {
    let rows = sqlx::query_as::<_, CategoryRow>(
        "SELECT id, server_id, name, position, created_at \
         FROM channel_categories \
         WHERE server_id = $1 \
         ORDER BY position, created_at",
    )
    .bind(server_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(CategoryRow::into_category).collect())
}

pub async fn insert_category(
    pool: &PgPool,
    server_id: Uuid,
    name: &str,
) -> Result<concord_shared::types::ChannelCategory, AppError> {
    let row = sqlx::query_as::<_, CategoryRow>(
        "INSERT INTO channel_categories (server_id, name, position) \
         VALUES ($1, $2, (SELECT COALESCE(MAX(position), -1) + 1 FROM channel_categories WHERE server_id = $1)) \
         RETURNING id, server_id, name, position, created_at",
    )
    .bind(server_id)
    .bind(name)
    .fetch_one(pool)
    .await?;

    Ok(row.into_category())
}

pub async fn rename_category_if_admin(
    pool: &PgPool,
    category_id: Uuid,
    user_id: Uuid,
    name: &str,
) -> Result<Option<concord_shared::types::ChannelCategory>, AppError> {
    let row = sqlx::query_as::<_, CategoryRow>(
        "UPDATE channel_categories SET name = $3 \
         WHERE id = $1 \
           AND EXISTS(\
               SELECT 1 FROM servers WHERE id = channel_categories.server_id AND owner_id = $2 \
               UNION ALL \
               SELECT 1 FROM server_members \
               WHERE server_id = channel_categories.server_id AND user_id = $2 AND role = 'admin'\
           ) \
         RETURNING id, server_id, name, position, created_at",
    )
    .bind(category_id)
    .bind(user_id)
    .bind(name)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(CategoryRow::into_category))
}

pub async fn delete_category_if_admin(
    pool: &PgPool,
    category_id: Uuid,
    user_id: Uuid,
) -> Result<bool, AppError> {
    let result = sqlx::query(
        "DELETE FROM channel_categories \
         WHERE id = $1 \
           AND EXISTS(\
               SELECT 1 FROM servers WHERE id = channel_categories.server_id AND owner_id = $2 \
               UNION ALL \
               SELECT 1 FROM server_members \
               WHERE server_id = channel_categories.server_id AND user_id = $2 AND role = 'admin'\
           )",
    )
    .bind(category_id)
    .bind(user_id)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}

pub async fn get_category_server_id(
    pool: &PgPool,
    category_id: Uuid,
) -> Result<Option<Uuid>, AppError> {
    let row = sqlx::query_scalar::<_, Uuid>(
        "SELECT server_id FROM channel_categories WHERE id = $1",
    )
    .bind(category_id)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

pub async fn reorder_channels(
    pool: &PgPool,
    server_id: Uuid,
    channels: &[(Uuid, Option<Uuid>, i32)],
    categories: &[(Uuid, i32)],
) -> Result<(), AppError> {
    let mut tx = pool.begin().await.map_err(|e| AppError::Internal(e.to_string()))?;

    for &(id, position) in categories {
        let affected = sqlx::query(
            "UPDATE channel_categories SET position = $2 WHERE id = $1 AND server_id = $3",
        )
        .bind(id)
        .bind(position)
        .bind(server_id)
        .execute(&mut *tx)
        .await?;

        if affected.rows_affected() == 0 {
            return Err(AppError::Validation(
                concord_shared::validation::ValidationError::InvalidValue {
                    field: "categories",
                    reason: "category not found in this server",
                },
            ));
        }
    }

    for &(id, category_id, position) in channels {
        let affected = sqlx::query(
            "UPDATE channels SET category_id = $2, position = $3 WHERE id = $1 AND server_id = $4",
        )
        .bind(id)
        .bind(category_id)
        .bind(position)
        .bind(server_id)
        .execute(&mut *tx)
        .await?;

        if affected.rows_affected() == 0 {
            return Err(AppError::Validation(
                concord_shared::validation::ValidationError::InvalidValue {
                    field: "channels",
                    reason: "channel not found in this server",
                },
            ));
        }
    }

    tx.commit().await.map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// dm_channels
// ---------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct DmChannelRow {
    id: Uuid,
    name: Option<String>,
    is_group: bool,
    owner_id: Option<Uuid>,
    created_at: DateTime<Utc>,
}

impl DmChannelRow {
    fn into_dm_channel(self) -> DmChannel {
        DmChannel {
            id: self.id,
            name: self.name,
            is_group: self.is_group,
            owner_id: self.owner_id,
            created_at: self.created_at,
        }
    }
}

#[derive(sqlx::FromRow)]
struct DmParticipantRow {
    user_id: Uuid,
    username: String,
    avatar_url: Option<String>,
}

impl DmParticipantRow {
    fn into_participant(self) -> DmParticipant {
        DmParticipant {
            user_id: self.user_id,
            username: self.username,
            avatar_url: self.avatar_url,
        }
    }
}

pub async fn user_exists(pool: &PgPool, user_id: Uuid) -> Result<bool, AppError> {
    let result = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM users WHERE id = $1)",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;

    Ok(result)
}

/// The public summary of a single user, or `None` if no such user exists.
pub async fn get_user_summary(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Option<UserSummary>, AppError> {
    let row = sqlx::query_as::<_, UserSummaryRow>(
        "SELECT id, username, avatar_url FROM users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(UserSummary::from))
}

#[derive(sqlx::FromRow)]
struct UserSummaryRow {
    id: Uuid,
    username: String,
    avatar_url: Option<String>,
}

impl From<UserSummaryRow> for UserSummary {
    fn from(row: UserSummaryRow) -> Self {
        UserSummary {
            id: row.id,
            username: row.username,
            avatar_url: row.avatar_url,
        }
    }
}

/// Find users whose username contains `query` (case-insensitive), excluding
/// `exclude` (the caller, so a search never offers a DM with yourself), capped
/// at `limit` and ordered username-first for a stable, alphabetical list.
///
/// `query` is matched as a substring with its LIKE metacharacters escaped, so a
/// caller typing `%` or `_` searches for those literal characters rather than
/// turning the pattern into a wildcard.
pub async fn search_users_by_username(
    pool: &PgPool,
    query: &str,
    exclude: Uuid,
    limit: i64,
) -> Result<Vec<UserSummary>, AppError> {
    let pattern = format!("%{}%", escape_like(query));
    let rows = sqlx::query_as::<_, UserSummaryRow>(
        "SELECT id, username, avatar_url \
         FROM users \
         WHERE id <> $1 AND username ILIKE $2 ESCAPE '\\' \
         ORDER BY username, id \
         LIMIT $3",
    )
    .bind(exclude)
    .bind(pattern)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(UserSummary::from).collect())
}

/// Look up a single user by exact (case-insensitive) username, excluding the
/// caller. "Add friend" needs an exact resolution, not the substring search's
/// ranked, capped list — an exact name can sort past the search cap and be
/// missed, which would read as "no such user".
pub async fn get_user_summary_by_username(
    pool: &PgPool,
    username: &str,
    exclude: Uuid,
) -> Result<Option<UserSummary>, AppError> {
    let row = sqlx::query_as::<_, UserSummaryRow>(
        "SELECT id, username, avatar_url \
         FROM users \
         WHERE id <> $1 AND lower(username) = lower($2)",
    )
    .bind(exclude)
    .bind(username)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(UserSummary::from))
}

/// Escape the `LIKE`/`ILIKE` metacharacters (`\`, `%`, `_`) in a user-supplied
/// search term so it is matched literally. The query uses `ESCAPE '\'`, so the
/// backslash must be escaped first to avoid double-unescaping.
fn escape_like(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Return the 1:1 DM channel between `user_a` and `user_b`, creating it (with
/// both `dm_members` rows) if none exists. The `bool` is true when a new
/// channel was inserted, false when an existing one was returned.
///
/// Find-or-create is inherently racy: two concurrent requests for the same
/// pair could both miss the lookup and each insert a channel, leaving two 1:1
/// DMs where the schema intends one (there is no unique constraint that would
/// catch it — the pair lives across two `dm_members` rows). We serialize on a
/// transaction-scoped advisory lock keyed on the *unordered* pair, so (A, B)
/// and (B, A) contend for the same lock; the loser then sees the winner's
/// channel in the lookup. The lock releases automatically at commit/rollback.
pub async fn find_or_create_dm_channel(
    pool: &PgPool,
    user_a: Uuid,
    user_b: Uuid,
) -> Result<(DmChannel, bool), AppError> {
    let mut tx = pool.begin().await.map_err(|e| AppError::Internal(e.to_string()))?;

    // Canonical (order-independent) key for the pair → one bigint lock id.
    sqlx::query(
        "SELECT pg_advisory_xact_lock(\
             ('x' || substr(\
                 md5(LEAST($1::text, $2::text) || '|' || GREATEST($1::text, $2::text)), \
                 1, 16))::bit(64)::bigint)",
    )
    .bind(user_a)
    .bind(user_b)
    .execute(&mut *tx)
    .await?;

    // A 1:1 DM is a non-group channel whose membership is exactly {a, b}: both
    // present and no third member.
    let existing = sqlx::query_as::<_, DmChannelRow>(
        "SELECT dc.id, dc.name, dc.is_group, dc.owner_id, dc.created_at \
         FROM dm_channels dc \
         WHERE dc.is_group = false \
           AND EXISTS(SELECT 1 FROM dm_members WHERE dm_channel_id = dc.id AND user_id = $1) \
           AND EXISTS(SELECT 1 FROM dm_members WHERE dm_channel_id = dc.id AND user_id = $2) \
           AND (SELECT count(*) FROM dm_members WHERE dm_channel_id = dc.id) = 2 \
         LIMIT 1",
    )
    .bind(user_a)
    .bind(user_b)
    .fetch_optional(&mut *tx)
    .await?;

    if let Some(row) = existing {
        tx.commit().await.map_err(|e| AppError::Internal(e.to_string()))?;
        return Ok((row.into_dm_channel(), false));
    }

    let channel = sqlx::query_as::<_, DmChannelRow>(
        "INSERT INTO dm_channels (is_group) VALUES (false) \
         RETURNING id, name, is_group, owner_id, created_at",
    )
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query("INSERT INTO dm_members (dm_channel_id, user_id) VALUES ($1, $2), ($1, $3)")
        .bind(channel.id)
        .bind(user_a)
        .bind(user_b)
        .execute(&mut *tx)
        .await?;

    tx.commit().await.map_err(|e| AppError::Internal(e.to_string()))?;

    Ok((channel.into_dm_channel(), true))
}

pub async fn list_dm_participants(
    pool: &PgPool,
    dm_channel_id: Uuid,
) -> Result<Vec<DmParticipant>, AppError> {
    let rows = sqlx::query_as::<_, DmParticipantRow>(
        "SELECT u.id AS user_id, u.username, u.avatar_url \
         FROM dm_members dm \
         JOIN users u ON u.id = dm.user_id \
         WHERE dm.dm_channel_id = $1 \
         ORDER BY dm.joined_at, u.id",
    )
    .bind(dm_channel_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(DmParticipantRow::into_participant).collect())
}

/// Participant row tagged with its DM channel, used to resolve every
/// participant for a *set* of channels in one query (see
/// [`list_dm_channels_for_user`]).
#[derive(sqlx::FromRow)]
struct DmParticipantWithChannelRow {
    dm_channel_id: Uuid,
    user_id: Uuid,
    username: String,
    avatar_url: Option<String>,
}

/// Every DM channel `user_id` belongs to, newest first, each with its
/// participants resolved — the list counterpart of the `DmChannelInfo` returned
/// by the create endpoints.
///
/// Participants for all channels are fetched in a single query and grouped in
/// memory, so the cost is two round-trips regardless of how many DMs the user
/// has rather than one per channel.
pub async fn list_dm_channels_for_user(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Vec<DmChannelInfo>, AppError> {
    let channels = sqlx::query_as::<_, DmChannelRow>(
        "SELECT dc.id, dc.name, dc.is_group, dc.owner_id, dc.created_at \
         FROM dm_channels dc \
         JOIN dm_members dm ON dm.dm_channel_id = dc.id \
         WHERE dm.user_id = $1 \
         ORDER BY dc.created_at DESC, dc.id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    if channels.is_empty() {
        return Ok(Vec::new());
    }

    let channel_ids: Vec<Uuid> = channels.iter().map(|c| c.id).collect();
    let participant_rows = sqlx::query_as::<_, DmParticipantWithChannelRow>(
        "SELECT dm.dm_channel_id, u.id AS user_id, u.username, u.avatar_url \
         FROM dm_members dm \
         JOIN users u ON u.id = dm.user_id \
         WHERE dm.dm_channel_id = ANY($1) \
         ORDER BY dm.joined_at, u.id",
    )
    .bind(&channel_ids)
    .fetch_all(pool)
    .await?;

    // Group participants by channel. The query's ORDER BY is global, but the
    // relative order within each channel is preserved as we append, so each
    // channel's participants stay ordered by (joined_at, id).
    let mut by_channel: std::collections::HashMap<Uuid, Vec<DmParticipant>> =
        std::collections::HashMap::with_capacity(channels.len());
    for row in participant_rows {
        by_channel.entry(row.dm_channel_id).or_default().push(DmParticipant {
            user_id: row.user_id,
            username: row.username,
            avatar_url: row.avatar_url,
        });
    }

    Ok(channels
        .into_iter()
        .map(|ch| DmChannelInfo {
            participants: by_channel.remove(&ch.id).unwrap_or_default(),
            id: ch.id,
            name: ch.name,
            is_group: ch.is_group,
            owner_id: ch.owner_id,
            created_at: ch.created_at,
        })
        .collect())
}

/// One DM channel `user_id` belongs to, enriched with its member count, a
/// last-message preview, and the caller's unread flag. The flat shape is folded
/// into a [`DmConversation`] by [`into_conversation`](Self::into_conversation).
#[derive(sqlx::FromRow)]
struct DmConversationRow {
    id: Uuid,
    name: Option<String>,
    is_group: bool,
    owner_id: Option<Uuid>,
    created_at: DateTime<Utc>,
    member_count: i64,
    last_message_id: Option<Uuid>,
    last_message_content: Option<String>,
    last_message_created_at: Option<DateTime<Utc>>,
    last_message_author_id: Option<Uuid>,
    last_message_author_username: Option<String>,
    last_message_author_avatar_url: Option<String>,
    unread: bool,
}

impl DmConversationRow {
    fn into_conversation(self, participants: Vec<DmParticipant>) -> DmConversation {
        // The last-message columns travel together: all present when the LATERAL
        // join found a row, all NULL when the conversation has no messages.
        let last_message = match (
            self.last_message_id,
            self.last_message_content,
            self.last_message_created_at,
        ) {
            (Some(id), Some(content), Some(created_at)) => {
                // author is NULL once the sender's account is deleted
                // (messages.author_id ON DELETE SET NULL), matching the
                // MessageWithAuthor convention of whole-or-absent authors.
                let author = match (self.last_message_author_id, self.last_message_author_username) {
                    (Some(id), Some(username)) => Some(MessageAuthor {
                        id,
                        username,
                        avatar_url: self.last_message_author_avatar_url,
                    }),
                    _ => None,
                };
                Some(DmLastMessage { id, author, content, created_at })
            }
            _ => None,
        };

        DmConversation {
            id: self.id,
            name: self.name,
            is_group: self.is_group,
            owner_id: self.owner_id,
            created_at: self.created_at,
            member_count: self.member_count,
            participants,
            last_message,
            unread: self.unread,
        }
    }
}

/// Every DM conversation `user_id` belongs to, newest-activity-first, each with
/// its members resolved plus a last-message preview and the caller's unread
/// flag — the data the DM conversation list (`GET /api/dms`) renders.
///
/// Like [`list_dm_channels_for_user`], participants for all conversations are
/// fetched in one batched query and grouped in memory, so the cost stays at two
/// round-trips regardless of how many DMs the user has.
pub async fn list_dm_conversations_for_user(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Vec<DmConversation>, AppError> {
    // Order by last activity (newest message, falling back to channel creation
    // for an empty conversation). The LATERAL join picks each channel's newest
    // message; `unread` is true when any message from another member is newer
    // than the caller's last read (or they have never read it).
    let rows = sqlx::query_as::<_, DmConversationRow>(
        "SELECT dc.id, dc.name, dc.is_group, dc.owner_id, dc.created_at, \
                (SELECT count(*) FROM dm_members WHERE dm_channel_id = dc.id) AS member_count, \
                lm.id AS last_message_id, \
                lm.content AS last_message_content, \
                lm.created_at AS last_message_created_at, \
                lm.author_id AS last_message_author_id, \
                au.username AS last_message_author_username, \
                au.avatar_url AS last_message_author_avatar_url, \
                EXISTS(\
                    SELECT 1 FROM messages m \
                    WHERE m.channel_id = dc.id \
                      AND m.author_id IS DISTINCT FROM $1 \
                      AND (dm.last_read_at IS NULL OR m.created_at > dm.last_read_at)\
                ) AS unread \
         FROM dm_channels dc \
         JOIN dm_members dm ON dm.dm_channel_id = dc.id AND dm.user_id = $1 \
         LEFT JOIN LATERAL (\
             SELECT m.id, m.content, m.created_at, m.author_id \
             FROM messages m \
             WHERE m.channel_id = dc.id \
             ORDER BY m.created_at DESC, m.id DESC \
             LIMIT 1\
         ) lm ON true \
         LEFT JOIN users au ON au.id = lm.author_id \
         ORDER BY COALESCE(lm.created_at, dc.created_at) DESC, dc.id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok(Vec::new());
    }

    let channel_ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    let participant_rows = sqlx::query_as::<_, DmParticipantWithChannelRow>(
        "SELECT dm.dm_channel_id, u.id AS user_id, u.username, u.avatar_url \
         FROM dm_members dm \
         JOIN users u ON u.id = dm.user_id \
         WHERE dm.dm_channel_id = ANY($1) \
         ORDER BY dm.joined_at, u.id",
    )
    .bind(&channel_ids)
    .fetch_all(pool)
    .await?;

    let mut by_channel: std::collections::HashMap<Uuid, Vec<DmParticipant>> =
        std::collections::HashMap::with_capacity(rows.len());
    for row in participant_rows {
        by_channel.entry(row.dm_channel_id).or_default().push(DmParticipant {
            user_id: row.user_id,
            username: row.username,
            avatar_url: row.avatar_url,
        });
    }

    Ok(rows
        .into_iter()
        .map(|row| {
            let participants = by_channel.remove(&row.id).unwrap_or_default();
            row.into_conversation(participants)
        })
        .collect())
}

/// Mark `dm_channel_id` read for `user_id` as of now, clearing its unread flag
/// until another member posts again. Returns `false` when no membership row was
/// updated (the caller isn't a member), so the route can answer `404`.
pub async fn mark_dm_read(
    pool: &PgPool,
    dm_channel_id: Uuid,
    user_id: Uuid,
) -> Result<bool, AppError> {
    let result =
        sqlx::query("UPDATE dm_members SET last_read_at = now() WHERE dm_channel_id = $1 AND user_id = $2")
            .bind(dm_channel_id)
            .bind(user_id)
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

// ---------------------------------------------------------------------------
// direct messages (group DMs)
// ---------------------------------------------------------------------------

pub async fn insert_dm_channel<'e, E>(
    executor: E,
    name: Option<&str>,
    owner_id: Uuid,
) -> Result<DmChannel, AppError>
where
    E: Executor<'e, Database = Postgres>,
{
    let row = sqlx::query_as::<_, DmChannelRow>(
        "INSERT INTO dm_channels (name, is_group, owner_id) \
         VALUES ($1, true, $2) \
         RETURNING id, name, is_group, owner_id, created_at",
    )
    .bind(name)
    .bind(owner_id)
    .fetch_one(executor)
    .await?;

    Ok(row.into_dm_channel())
}

pub async fn insert_dm_member<'e, E>(
    executor: E,
    dm_channel_id: Uuid,
    user_id: Uuid,
) -> Result<(), AppError>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query("INSERT INTO dm_members (dm_channel_id, user_id) VALUES ($1, $2)")
        .bind(dm_channel_id)
        .bind(user_id)
        .execute(executor)
        .await?;

    Ok(())
}

/// Return the subset of `ids` that exist in `users`. Used to reject group-DM
/// creation that names recipients who aren't real accounts.
pub async fn existing_user_ids(pool: &PgPool, ids: &[Uuid]) -> Result<Vec<Uuid>, AppError> {
    let rows = sqlx::query_scalar::<_, Uuid>("SELECT id FROM users WHERE id = ANY($1)")
        .bind(ids)
        .fetch_all(pool)
        .await?;

    Ok(rows)
}

/// Fetch a group DM by id. Returns `None` for a missing channel or a 1:1 DM —
/// the member-management endpoints operate on groups only.
pub async fn get_group_dm(
    pool: &PgPool,
    dm_channel_id: Uuid,
) -> Result<Option<DmChannel>, AppError> {
    let row = sqlx::query_as::<_, DmChannelRow>(
        "SELECT id, name, is_group, owner_id, created_at \
         FROM dm_channels WHERE id = $1 AND is_group = true",
    )
    .bind(dm_channel_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(DmChannelRow::into_dm_channel))
}

pub async fn is_dm_member(
    pool: &PgPool,
    dm_channel_id: Uuid,
    user_id: Uuid,
) -> Result<bool, AppError> {
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM dm_members WHERE dm_channel_id = $1 AND user_id = $2)",
    )
    .bind(dm_channel_id)
    .bind(user_id)
    .fetch_one(pool)
    .await?;

    Ok(exists)
}

/// Every member's user id for a DM channel. A DM message fans out to exactly
/// these ids (and only these), so the broadcast reaches participants alone.
pub async fn list_dm_member_ids(
    pool: &PgPool,
    dm_channel_id: Uuid,
) -> Result<Vec<Uuid>, AppError> {
    let ids = sqlx::query_scalar::<_, Uuid>(
        "SELECT user_id FROM dm_members WHERE dm_channel_id = $1",
    )
    .bind(dm_channel_id)
    .fetch_all(pool)
    .await?;

    Ok(ids)
}

pub async fn dm_member_count(pool: &PgPool, dm_channel_id: Uuid) -> Result<i64, AppError> {
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM dm_members WHERE dm_channel_id = $1",
    )
    .bind(dm_channel_id)
    .fetch_one(pool)
    .await?;

    Ok(count)
}

/// Take a transaction-scoped advisory lock keyed on a single DM channel so that
/// every membership mutation on that channel — adds and removes alike —
/// serializes against the others. The lock releases automatically at
/// commit/rollback. This is the channel-scoped counterpart to the unordered
/// *pair* lock in [`find_or_create_dm_channel`]; both derive a bigint lock id
/// the same way (`md5` → first 16 hex digits → `bit(64)` → `bigint`).
async fn lock_dm_channel<'e, E>(executor: E, dm_channel_id: Uuid) -> Result<(), AppError>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query(
        "SELECT pg_advisory_xact_lock(\
             ('x' || substr(md5($1::text), 1, 16))::bit(64)::bigint)",
    )
    .bind(dm_channel_id)
    .execute(executor)
    .await?;

    Ok(())
}

/// Outcome of an attempt to add a member to a group DM, decided inside the
/// serialized critical section so the caller can map it to a status code.
pub enum AddMemberOutcome {
    Added,
    AlreadyMember,
    Full,
}

/// Add `user_id` to a group DM atomically: the duplicate-membership check, the
/// `max` head-count check, and the insert all run in one transaction behind a
/// per-channel advisory lock (mirrors [`find_or_create_dm_channel`]). Without
/// the lock two concurrent adds can both read `count < max` and both insert,
/// pushing the group over its cap; the `dm_members` PK only stops re-adding the
/// *same* user, not over-counting distinct users.
pub async fn add_dm_member_checked(
    pool: &PgPool,
    dm_channel_id: Uuid,
    user_id: Uuid,
    max: usize,
) -> Result<AddMemberOutcome, AppError> {
    let mut tx = pool.begin().await.map_err(|e| AppError::Internal(e.to_string()))?;

    // One bigint lock id per channel; serializes concurrent adds on this DM.
    lock_dm_channel(&mut *tx, dm_channel_id).await?;

    let already = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM dm_members WHERE dm_channel_id = $1 AND user_id = $2)",
    )
    .bind(dm_channel_id)
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await?;
    if already {
        tx.rollback().await.map_err(|e| AppError::Internal(e.to_string()))?;
        return Ok(AddMemberOutcome::AlreadyMember);
    }

    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM dm_members WHERE dm_channel_id = $1",
    )
    .bind(dm_channel_id)
    .fetch_one(&mut *tx)
    .await?;
    if count as usize >= max {
        tx.rollback().await.map_err(|e| AppError::Internal(e.to_string()))?;
        return Ok(AddMemberOutcome::Full);
    }

    insert_dm_member(&mut *tx, dm_channel_id, user_id).await?;

    tx.commit().await.map_err(|e| AppError::Internal(e.to_string()))?;

    Ok(AddMemberOutcome::Added)
}

/// Remove `target` from a group DM and repair invariants in one transaction:
/// if the departing member owned the group, ownership passes to the
/// earliest-joined survivor; if no members remain, the empty channel is
/// deleted (its rows cascade). Returns `false` if `target` wasn't a member.
pub async fn remove_dm_member(
    pool: &PgPool,
    dm_channel_id: Uuid,
    target: Uuid,
) -> Result<bool, AppError> {
    let mut tx = pool.begin().await.map_err(|e| AppError::Internal(e.to_string()))?;

    // Serialize on the same per-channel lock the add path takes. Without it two
    // concurrent leaves each delete their own row, then read a snapshot that
    // still shows the other member: both take the `Some(survivor)` branch, so
    // the now-empty channel is never deleted and ownership can land on a member
    // who is also leaving. The lock also orders leaves against concurrent adds.
    lock_dm_channel(&mut *tx, dm_channel_id).await?;

    let deleted = sqlx::query("DELETE FROM dm_members WHERE dm_channel_id = $1 AND user_id = $2")
        .bind(dm_channel_id)
        .bind(target)
        .execute(&mut *tx)
        .await?;

    if deleted.rows_affected() == 0 {
        tx.rollback().await.map_err(|e| AppError::Internal(e.to_string()))?;
        return Ok(false);
    }

    let next_owner = sqlx::query_scalar::<_, Uuid>(
        "SELECT user_id FROM dm_members WHERE dm_channel_id = $1 \
         ORDER BY joined_at, user_id LIMIT 1",
    )
    .bind(dm_channel_id)
    .fetch_optional(&mut *tx)
    .await?;

    match next_owner {
        // Last member left: drop the now-empty channel.
        None => {
            sqlx::query("DELETE FROM dm_channels WHERE id = $1")
                .bind(dm_channel_id)
                .execute(&mut *tx)
                .await?;
        }
        // Reassign ownership only if the member who left was the owner.
        Some(survivor) => {
            sqlx::query("UPDATE dm_channels SET owner_id = $2 WHERE id = $1 AND owner_id = $3")
                .bind(dm_channel_id)
                .bind(survivor)
                .bind(target)
                .execute(&mut *tx)
                .await?;
        }
    }

    tx.commit().await.map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Friendships
// ---------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct FriendshipStateRow {
    id: Uuid,
    requester_id: Uuid,
    status: String,
}

#[derive(sqlx::FromRow)]
struct NewFriendshipRow {
    id: Uuid,
    created_at: DateTime<Utc>,
}

/// One accepted friend: the *other* user plus when the friendship was made.
#[derive(sqlx::FromRow)]
struct FriendListRow {
    id: Uuid,
    username: String,
    avatar_url: Option<String>,
    since: DateTime<Utc>,
}

/// One pending request: the friendship row id, the *other* user, and when it
/// was sent.
#[derive(sqlx::FromRow)]
struct FriendRequestRow {
    id: Uuid,
    user_id: Uuid,
    username: String,
    avatar_url: Option<String>,
    created_at: DateTime<Utc>,
}

impl FriendRequestRow {
    fn into_request(self, direction: FriendRequestDirection) -> FriendRequest {
        FriendRequest {
            id: self.id,
            user: UserSummary {
                id: self.user_id,
                username: self.username,
                avatar_url: self.avatar_url,
            },
            direction,
            created_at: self.created_at,
        }
    }
}

/// Outcome of sending a friend request, decided under the canonical pair lock so
/// two concurrent sends can never both insert (or insert a mirror row).
pub enum SendFriendOutcome {
    /// A new pending request was created. Carries the row id and timestamp so
    /// the handler can build the response and notify the addressee.
    Sent { id: Uuid, created_at: DateTime<Utc> },
    /// A reverse pending request already existed and was accepted instead, so
    /// the two are now friends.
    Accepted { id: Uuid },
    /// The caller already has a pending request out to this user.
    AlreadyRequested,
    /// The two are already friends.
    AlreadyFriends,
}

/// Send a friend request from `requester` to `addressee`, or — if `addressee`
/// already has a pending request out to `requester` — accept that one instead.
///
/// Like [`find_or_create_dm_channel`], the check-then-write runs under a
/// transaction-scoped advisory lock keyed on the *unordered* pair, so a
/// simultaneous send in the opposite direction can't slip a duplicate (or the
/// mirror row the unique index forbids) past the existence check.
pub async fn send_friend_request(
    pool: &PgPool,
    requester: Uuid,
    addressee: Uuid,
) -> Result<SendFriendOutcome, AppError> {
    let mut tx = pool.begin().await.map_err(|e| AppError::Internal(e.to_string()))?;

    // Canonical (order-independent) key for the pair → one bigint lock id.
    sqlx::query(
        "SELECT pg_advisory_xact_lock(\
             ('x' || substr(\
                 md5(LEAST($1::text, $2::text) || '|' || GREATEST($1::text, $2::text)), \
                 1, 16))::bit(64)::bigint)",
    )
    .bind(requester)
    .bind(addressee)
    .execute(&mut *tx)
    .await?;

    let existing = sqlx::query_as::<_, FriendshipStateRow>(
        "SELECT id, requester_id, status FROM friendships \
         WHERE (requester_id = $1 AND addressee_id = $2) \
            OR (requester_id = $2 AND addressee_id = $1)",
    )
    .bind(requester)
    .bind(addressee)
    .fetch_optional(&mut *tx)
    .await?;

    let outcome = match existing {
        Some(row) if row.status == "accepted" => SendFriendOutcome::AlreadyFriends,
        Some(row) if row.requester_id == requester => SendFriendOutcome::AlreadyRequested,
        // A pending request from the other side: accepting it is the natural,
        // friendlier resolution rather than rejecting the duplicate.
        Some(row) => {
            sqlx::query(
                "UPDATE friendships SET status = 'accepted', updated_at = now() WHERE id = $1",
            )
            .bind(row.id)
            .execute(&mut *tx)
            .await?;
            SendFriendOutcome::Accepted { id: row.id }
        }
        None => {
            let row = sqlx::query_as::<_, NewFriendshipRow>(
                "INSERT INTO friendships (requester_id, addressee_id) \
                 VALUES ($1, $2) RETURNING id, created_at",
            )
            .bind(requester)
            .bind(addressee)
            .fetch_one(&mut *tx)
            .await?;
            SendFriendOutcome::Sent { id: row.id, created_at: row.created_at }
        }
    };

    tx.commit().await.map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(outcome)
}

/// Accept the pending request `request_id`, but only if `caller` is its
/// addressee. Returns the requester's id on success (so the handler can resolve
/// the new friend and notify them), or `None` if the request doesn't exist,
/// isn't pending, or isn't addressed to the caller.
pub async fn accept_friend_request(
    pool: &PgPool,
    request_id: Uuid,
    caller: Uuid,
) -> Result<Option<Uuid>, AppError> {
    let requester = sqlx::query_scalar::<_, Uuid>(
        "UPDATE friendships SET status = 'accepted', updated_at = now() \
         WHERE id = $1 AND addressee_id = $2 AND status = 'pending' \
         RETURNING requester_id",
    )
    .bind(request_id)
    .bind(caller)
    .fetch_optional(pool)
    .await?;

    Ok(requester)
}

/// Delete a pending request `request_id` that `caller` is part of — rejecting it
/// (caller is the addressee) or cancelling it (caller is the requester). Returns
/// `(requester_id, addressee_id)` on success so the handler can notify the other
/// party, or `None` if there is no such pending request involving the caller.
pub async fn delete_friend_request(
    pool: &PgPool,
    request_id: Uuid,
    caller: Uuid,
) -> Result<Option<(Uuid, Uuid)>, AppError> {
    let row = sqlx::query_as::<_, (Uuid, Uuid)>(
        "DELETE FROM friendships \
         WHERE id = $1 AND status = 'pending' AND $2 IN (requester_id, addressee_id) \
         RETURNING requester_id, addressee_id",
    )
    .bind(request_id)
    .bind(caller)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

/// Remove the accepted friendship between `caller` and `other` (either side may
/// unfriend). Returns `true` if a friendship was removed, `false` if the two
/// weren't friends.
pub async fn remove_friend(
    pool: &PgPool,
    caller: Uuid,
    other: Uuid,
) -> Result<bool, AppError> {
    let deleted = sqlx::query_scalar::<_, Uuid>(
        "DELETE FROM friendships \
         WHERE status = 'accepted' \
           AND ((requester_id = $1 AND addressee_id = $2) \
             OR (requester_id = $2 AND addressee_id = $1)) \
         RETURNING id",
    )
    .bind(caller)
    .bind(other)
    .fetch_optional(pool)
    .await?;

    Ok(deleted.is_some())
}

/// The caller's accepted friends, alphabetically by username. Each row is the
/// other user plus the timestamp the friendship was established; the handler
/// overlays live presence.
pub async fn list_friends(
    pool: &PgPool,
    caller: Uuid,
) -> Result<Vec<(UserSummary, DateTime<Utc>)>, AppError> {
    let rows = sqlx::query_as::<_, FriendListRow>(
        "SELECT u.id, u.username, u.avatar_url, f.updated_at AS since \
         FROM friendships f \
         JOIN users u ON u.id = CASE WHEN f.requester_id = $1 \
                                     THEN f.addressee_id ELSE f.requester_id END \
         WHERE f.status = 'accepted' AND (f.requester_id = $1 OR f.addressee_id = $1) \
         ORDER BY u.username, u.id",
    )
    .bind(caller)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            (
                UserSummary { id: r.id, username: r.username, avatar_url: r.avatar_url },
                r.since,
            )
        })
        .collect())
}

/// The caller's pending requests, split into incoming (addressed to them) and
/// outgoing (sent by them), each newest-first.
pub async fn list_friend_requests(
    pool: &PgPool,
    caller: Uuid,
) -> Result<FriendRequests, AppError> {
    let incoming = sqlx::query_as::<_, FriendRequestRow>(
        "SELECT f.id, u.id AS user_id, u.username, u.avatar_url, f.created_at \
         FROM friendships f \
         JOIN users u ON u.id = f.requester_id \
         WHERE f.status = 'pending' AND f.addressee_id = $1 \
         ORDER BY f.created_at DESC",
    )
    .bind(caller)
    .fetch_all(pool)
    .await?;

    let outgoing = sqlx::query_as::<_, FriendRequestRow>(
        "SELECT f.id, u.id AS user_id, u.username, u.avatar_url, f.created_at \
         FROM friendships f \
         JOIN users u ON u.id = f.addressee_id \
         WHERE f.status = 'pending' AND f.requester_id = $1 \
         ORDER BY f.created_at DESC",
    )
    .bind(caller)
    .fetch_all(pool)
    .await?;

    Ok(FriendRequests {
        incoming: incoming
            .into_iter()
            .map(|r| r.into_request(FriendRequestDirection::Incoming))
            .collect(),
        outgoing: outgoing
            .into_iter()
            .map(|r| r.into_request(FriendRequestDirection::Outgoing))
            .collect(),
    })
}
