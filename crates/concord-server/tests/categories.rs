//! Integration tests for the channel-category list endpoint (issue #30):
//! `GET /api/servers/{id}/categories`.
//!
//! Each test builds its own pool/runtime via `setup_pool` rather than sharing a
//! static one — see `helpers::setup_pool` for why.

mod helpers;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sqlx::PgPool;
use uuid::Uuid;

use helpers::{app_with_pool, authed_get, seed_server_channel, seed_user, send_json, setup_pool};

fn uri(server_id: Uuid) -> String {
    format!("/api/servers/{server_id}/categories")
}

/// Insert a category with an explicit `position` so ordering is deterministic.
async fn seed_category(pool: &PgPool, server_id: Uuid, name: &str, position: i32) -> Uuid {
    sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO channel_categories (server_id, name, position) \
         VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(server_id)
    .bind(name)
    .bind(position)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn unauthenticated_request_is_rejected() {
    let app = app_with_pool(setup_pool().await);
    let req = Request::builder()
        .method("GET")
        .uri(uri(Uuid::new_v4()))
        .body(Body::empty())
        .unwrap();

    let (status, _) = send_json(&app, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn nonexistent_server_returns_404() {
    let pool = setup_pool().await;
    let (user, _) = seed_user(&pool, None).await;

    let (status, _) = send_json(
        &app_with_pool(pool.clone()),
        authed_get(&uri(Uuid::new_v4()), user),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn non_member_is_forbidden() {
    let pool = setup_pool().await;
    let (owner, _) = seed_user(&pool, None).await;
    let (server, _channel) = seed_server_channel(&pool, owner).await;
    let (outsider, _) = seed_user(&pool, None).await;

    let (status, _) = send_json(
        &app_with_pool(pool.clone()),
        authed_get(&uri(server), outsider),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn lists_categories_ordered_by_position() {
    let pool = setup_pool().await;
    let (owner, _) = seed_user(&pool, None).await;
    let (server, _channel) = seed_server_channel(&pool, owner).await;
    // Insert out of order; the endpoint must return them ordered by position.
    seed_category(&pool, server, "Voice", 2).await;
    seed_category(&pool, server, "Text", 0).await;
    seed_category(&pool, server, "Info", 1).await;

    let (status, body) =
        send_json(&app_with_pool(pool.clone()), authed_get(&uri(server), owner)).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["Text", "Info", "Voice"]);
}

#[tokio::test]
async fn empty_when_no_categories() {
    let pool = setup_pool().await;
    let (owner, _) = seed_user(&pool, None).await;
    let (server, _channel) = seed_server_channel(&pool, owner).await;

    let (status, body) =
        send_json(&app_with_pool(pool.clone()), authed_get(&uri(server), owner)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.as_array().unwrap().is_empty());
}
