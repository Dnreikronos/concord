//! Integration tests for the message-history endpoint (issue #18):
//! `GET /api/channels/{id}/messages`.
//!
//! Each test builds its own pool/runtime via `setup_pool` rather than sharing a
//! static one — see `helpers::setup_pool` for why.

mod helpers;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;
use uuid::Uuid;

use helpers::{
    app_with_pool, authed_get, seed_dm_channel, seed_message_at, seed_messages_bulk,
    seed_server_channel, seed_server_member, seed_user, setup_pool,
};

async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

fn uri(channel_id: Uuid) -> String {
    format!("/api/channels/{channel_id}/messages")
}

#[tokio::test]
async fn unauthenticated_request_is_rejected() {
    let app = app_with_pool(setup_pool().await);
    let req = Request::builder()
        .method("GET")
        .uri(uri(Uuid::new_v4()))
        .body(Body::empty())
        .unwrap();

    let (status, _) = send(app, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn nonexistent_channel_returns_404() {
    let pool = setup_pool().await;
    let (user, _) = seed_user(&pool, None).await;

    let (status, _) = send(
        app_with_pool(pool.clone()),
        authed_get(&uri(Uuid::new_v4()), user),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn non_member_is_forbidden() {
    let pool = setup_pool().await;
    let (owner, _) = seed_user(&pool, None).await;
    let (_server, channel) = seed_server_channel(&pool, owner).await;
    let (outsider, _) = seed_user(&pool, None).await;

    let (status, _) = send(
        app_with_pool(pool.clone()),
        authed_get(&uri(channel), outsider),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn server_member_can_read_history() {
    let pool = setup_pool().await;
    let (owner, _) = seed_user(&pool, None).await;
    let (server, channel) = seed_server_channel(&pool, owner).await;
    let (member, _) = seed_user(&pool, None).await;
    seed_server_member(&pool, server, member, "member").await;
    seed_message_at(&pool, channel, Some(owner), "hello", Utc::now()).await;

    let (status, body) = send(
        app_with_pool(pool.clone()),
        authed_get(&uri(channel), member),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["content"], "hello");
}

#[tokio::test]
async fn returns_messages_newest_first_with_author() {
    let pool = setup_pool().await;
    let avatar = "https://cdn.example/a.png";
    let (user, username) = seed_user(&pool, Some(avatar)).await;
    let (_server, channel) = seed_server_channel(&pool, user).await;

    let base = Utc::now();
    seed_message_at(&pool, channel, Some(user), "first", base).await;
    seed_message_at(
        &pool,
        channel,
        Some(user),
        "second",
        base + Duration::seconds(1),
    )
    .await;
    seed_message_at(
        &pool,
        channel,
        Some(user),
        "third",
        base + Duration::seconds(2),
    )
    .await;

    let (status, body) = send(app_with_pool(pool.clone()), authed_get(&uri(channel), user)).await;
    assert_eq!(status, StatusCode::OK);

    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 3);
    // reverse chronological
    assert_eq!(arr[0]["content"], "third");
    assert_eq!(arr[1]["content"], "second");
    assert_eq!(arr[2]["content"], "first");

    // author info embedded
    let author = &arr[0]["author"];
    assert_eq!(author["id"], user.to_string());
    assert_eq!(author["username"], username);
    assert_eq!(author["avatar_url"], avatar);
}

#[tokio::test]
async fn author_is_absent_for_deleted_account() {
    let pool = setup_pool().await;
    let (member, _) = seed_user(&pool, None).await;
    let (_server, channel) = seed_server_channel(&pool, member).await;
    // author_id NULL mimics the ON DELETE SET NULL state after the author is gone.
    seed_message_at(&pool, channel, None, "ghost", Utc::now()).await;

    let (status, body) = send(
        app_with_pool(pool.clone()),
        authed_get(&uri(channel), member),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["content"], "ghost");
    assert!(
        arr[0].get("author").is_none(),
        "author must be omitted when the account was deleted"
    );
}

#[tokio::test]
async fn limit_defaults_to_50_and_caps_at_100() {
    let pool = setup_pool().await;
    let (user, _) = seed_user(&pool, None).await;
    let (_server, channel) = seed_server_channel(&pool, user).await;
    seed_messages_bulk(&pool, channel, Some(user), 105, Utc::now()).await;

    // No limit → default of 50.
    let (_s, body) = send(app_with_pool(pool.clone()), authed_get(&uri(channel), user)).await;
    assert_eq!(body.as_array().unwrap().len(), 50);

    // Explicit small limit is honored.
    let u = format!("{}?limit=10", uri(channel));
    let (_s, body) = send(app_with_pool(pool.clone()), authed_get(&u, user)).await;
    assert_eq!(body.as_array().unwrap().len(), 10);

    // Above the cap clamps down to 100.
    let u = format!("{}?limit=1000", uri(channel));
    let (_s, body) = send(app_with_pool(pool.clone()), authed_get(&u, user)).await;
    assert_eq!(body.as_array().unwrap().len(), 100);

    // Zero clamps up to 1.
    let u = format!("{}?limit=0", uri(channel));
    let (_s, body) = send(app_with_pool(pool.clone()), authed_get(&u, user)).await;
    assert_eq!(body.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn cursor_paginates_backwards() {
    let pool = setup_pool().await;
    let (user, _) = seed_user(&pool, None).await;
    let (_server, channel) = seed_server_channel(&pool, user).await;

    let base = Utc::now();
    let mut m = Vec::new();
    for i in 0..5i64 {
        let id = seed_message_at(
            &pool,
            channel,
            Some(user),
            &format!("m{i}"),
            base + Duration::seconds(i),
        )
        .await;
        m.push(id);
    }
    // m[0] = oldest, m[4] = newest.

    // Page 1: the two newest.
    let u = format!("{}?limit=2", uri(channel));
    let (_s, body) = send(app_with_pool(pool.clone()), authed_get(&u, user)).await;
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["id"], m[4].to_string());
    assert_eq!(arr[1]["id"], m[3].to_string());

    // Page 2: strictly older than m[3].
    let u = format!("{}?limit=2&before={}", uri(channel), m[3]);
    let (_s, body) = send(app_with_pool(pool.clone()), authed_get(&u, user)).await;
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["id"], m[2].to_string());
    assert_eq!(arr[1]["id"], m[1].to_string());

    // Page 3: only the oldest remains.
    let u = format!("{}?limit=2&before={}", uri(channel), m[1]);
    let (_s, body) = send(app_with_pool(pool.clone()), authed_get(&u, user)).await;
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], m[0].to_string());

    // Page 4: nothing older than the oldest.
    let u = format!("{}?before={}", uri(channel), m[0]);
    let (_s, body) = send(app_with_pool(pool.clone()), authed_get(&u, user)).await;
    assert!(body.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn unknown_before_cursor_yields_empty() {
    let pool = setup_pool().await;
    let (user, _) = seed_user(&pool, None).await;
    let (_server, channel) = seed_server_channel(&pool, user).await;
    seed_message_at(&pool, channel, Some(user), "hi", Utc::now()).await;

    // A cursor that isn't a message in this channel must not leak rows.
    let u = format!("{}?before={}", uri(channel), Uuid::new_v4());
    let (status, body) = send(app_with_pool(pool.clone()), authed_get(&u, user)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn dm_member_reads_empty_history() {
    let pool = setup_pool().await;
    let (a, _) = seed_user(&pool, None).await;
    let (b, _) = seed_user(&pool, None).await;
    let dm = seed_dm_channel(&pool, &[a, b]).await;

    let (status, body) = send(app_with_pool(pool.clone()), authed_get(&uri(dm), a)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn dm_non_member_is_forbidden() {
    let pool = setup_pool().await;
    let (a, _) = seed_user(&pool, None).await;
    let (b, _) = seed_user(&pool, None).await;
    let (outsider, _) = seed_user(&pool, None).await;
    let dm = seed_dm_channel(&pool, &[a, b]).await;

    let (status, _) = send(app_with_pool(pool.clone()), authed_get(&uri(dm), outsider)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
