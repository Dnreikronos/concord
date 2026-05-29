//! Schema-level tests for migration 0005 (issue #23), which lets a DM channel
//! id live in `messages.channel_id`.
//!
//! Replacing the single-table `messages_channel_id_fkey` with triggers means
//! the FK's two guarantees — referential existence and ON DELETE CASCADE — are
//! now app-schema code rather than a built-in. Nothing else exercises them, so
//! these tests pin both, for server channels and DM channels alike.

mod helpers;

use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;

use helpers::{seed_dm_channel, seed_message_at, seed_server_channel, seed_user, setup_pool};

async fn message_exists(pool: &PgPool, id: Uuid) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM messages WHERE id = $1)")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn dm_channel_id_is_accepted_in_messages() {
    // The whole point of the migration: a dm_channels id, which the old FK
    // rejected, now inserts cleanly into the shared messages table.
    let pool = setup_pool().await;
    let a = seed_user(&pool, None).await.0;
    let b = seed_user(&pool, None).await.0;
    let dm = seed_dm_channel(&pool, &[a, b]).await;

    let msg = seed_message_at(&pool, dm, Some(a), "hi", Utc::now()).await;
    assert!(message_exists(&pool, msg).await);
}

#[tokio::test]
async fn unknown_channel_id_is_rejected_in_messages() {
    // The existence trigger stands in for the dropped FK: a channel_id that
    // names neither a server channel nor a DM channel must be refused.
    let pool = setup_pool().await;
    let author = seed_user(&pool, None).await.0;

    let result = sqlx::query(
        "INSERT INTO messages (channel_id, author_id, content) VALUES ($1, $2, 'orphan')",
    )
    .bind(Uuid::new_v4())
    .bind(author)
    .execute(&pool)
    .await;

    assert!(result.is_err(), "message with no backing channel must be rejected");
}

#[tokio::test]
async fn deleting_server_channel_cascades_its_messages() {
    let pool = setup_pool().await;
    let owner = seed_user(&pool, None).await.0;
    let (_server, channel) = seed_server_channel(&pool, owner).await;
    let msg = seed_message_at(&pool, channel, Some(owner), "doomed", Utc::now()).await;

    sqlx::query("DELETE FROM channels WHERE id = $1")
        .bind(channel)
        .execute(&pool)
        .await
        .unwrap();

    assert!(!message_exists(&pool, msg).await, "channel delete must drop its messages");
}

#[tokio::test]
async fn deleting_server_cascades_through_channels_to_messages() {
    // Deleting a server cascades (FK) to its channels, whose deletion must in
    // turn fire the cascade trigger that clears their messages.
    let pool = setup_pool().await;
    let owner = seed_user(&pool, None).await.0;
    let (server, channel) = seed_server_channel(&pool, owner).await;
    let msg = seed_message_at(&pool, channel, Some(owner), "doomed", Utc::now()).await;

    sqlx::query("DELETE FROM servers WHERE id = $1")
        .bind(server)
        .execute(&pool)
        .await
        .unwrap();

    assert!(!message_exists(&pool, msg).await, "server delete must drop channel messages");
}

#[tokio::test]
async fn deleting_dm_channel_cascades_its_messages() {
    let pool = setup_pool().await;
    let a = seed_user(&pool, None).await.0;
    let b = seed_user(&pool, None).await.0;
    let dm = seed_dm_channel(&pool, &[a, b]).await;
    let msg = seed_message_at(&pool, dm, Some(a), "doomed", Utc::now()).await;

    sqlx::query("DELETE FROM dm_channels WHERE id = $1")
        .bind(dm)
        .execute(&pool)
        .await
        .unwrap();

    assert!(!message_exists(&pool, msg).await, "dm channel delete must drop its messages");
}
