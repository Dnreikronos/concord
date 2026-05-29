//! Integration tests for the 1:1 DM-creation endpoint (issue #21):
//! `POST /api/dms`.
//!
//! Each test builds its own pool/runtime via `setup_pool` rather than sharing a
//! static one — see `helpers::setup_pool` for why.

mod helpers;

use std::collections::HashSet;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

use helpers::{app_with_pool, auth_header, authed_post, seed_dm_channel, seed_user, setup_pool};

async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

fn create_dm_body(recipient_id: Uuid) -> String {
    json!({ "recipient_id": recipient_id }).to_string()
}

/// The set of participant `user_id`s in a `DmChannelInfo` response body.
fn participant_ids(body: &Value) -> HashSet<Uuid> {
    body["participants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["user_id"].as_str().unwrap().parse().unwrap())
        .collect()
}

#[tokio::test]
async fn unauthenticated_request_is_rejected() {
    let app = app_with_pool(setup_pool().await);
    let req = Request::builder()
        .method("POST")
        .uri("/api/dms")
        .header("content-type", "application/json")
        .body(Body::from(create_dm_body(Uuid::new_v4())))
        .unwrap();

    let (status, _) = send(app, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn dm_with_self_is_rejected() {
    let pool = setup_pool().await;
    let (user, _) = seed_user(&pool, None).await;

    let (status, body) = send(
        app_with_pool(pool.clone()),
        authed_post("/api/dms", user, &create_dm_body(user)),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap().contains("recipient_id"),
        "got: {body}"
    );
}

#[tokio::test]
async fn unknown_recipient_returns_404() {
    let pool = setup_pool().await;
    let (user, _) = seed_user(&pool, None).await;

    let (status, _) = send(
        app_with_pool(pool.clone()),
        authed_post("/api/dms", user, &create_dm_body(Uuid::new_v4())),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn creates_dm_with_both_participants() {
    let pool = setup_pool().await;
    let (alice, alice_name) = seed_user(&pool, None).await;
    let (bob, bob_name) = seed_user(&pool, None).await;

    let (status, body) = send(
        app_with_pool(pool.clone()),
        authed_post("/api/dms", alice, &create_dm_body(bob)),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["is_group"], json!(false));
    // A 1:1 DM carries no name or owner; both are skipped when serializing.
    assert!(body.get("name").is_none(), "got: {body}");
    assert!(body.get("owner_id").is_none(), "got: {body}");
    assert!(body["id"].as_str().unwrap().parse::<Uuid>().is_ok());

    assert_eq!(participant_ids(&body), HashSet::from([alice, bob]));

    let names: HashSet<String> = body["participants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["username"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, HashSet::from([alice_name, bob_name]));
}

#[tokio::test]
async fn participant_avatar_is_surfaced() {
    let pool = setup_pool().await;
    let (alice, _) = seed_user(&pool, Some("https://cdn.example.com/a.png")).await;
    let (bob, _) = seed_user(&pool, None).await;

    let (status, body) = send(
        app_with_pool(pool.clone()),
        authed_post("/api/dms", alice, &create_dm_body(bob)),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);

    let participants = body["participants"].as_array().unwrap();
    let alice_p = participants
        .iter()
        .find(|p| p["user_id"].as_str().unwrap().parse::<Uuid>().unwrap() == alice)
        .unwrap();
    let bob_p = participants
        .iter()
        .find(|p| p["user_id"].as_str().unwrap().parse::<Uuid>().unwrap() == bob)
        .unwrap();

    assert_eq!(alice_p["avatar_url"], json!("https://cdn.example.com/a.png"));
    // Bob has no avatar, so the field is omitted entirely.
    assert!(bob_p.get("avatar_url").is_none(), "got: {body}");
}

#[tokio::test]
async fn repeat_request_reuses_channel_and_returns_200() {
    let pool = setup_pool().await;
    let (alice, _) = seed_user(&pool, None).await;
    let (bob, _) = seed_user(&pool, None).await;

    let (status, first) = send(
        app_with_pool(pool.clone()),
        authed_post("/api/dms", alice, &create_dm_body(bob)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let first_id = first["id"].as_str().unwrap().to_owned();

    // Second call for the same pair: found, not created.
    let (status, second) = send(
        app_with_pool(pool.clone()),
        authed_post("/api/dms", alice, &create_dm_body(bob)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(second["id"].as_str().unwrap(), first_id);
}

#[tokio::test]
async fn reverse_direction_finds_same_channel() {
    let pool = setup_pool().await;
    let (alice, _) = seed_user(&pool, None).await;
    let (bob, _) = seed_user(&pool, None).await;

    let (status, first) = send(
        app_with_pool(pool.clone()),
        authed_post("/api/dms", alice, &create_dm_body(bob)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let first_id = first["id"].as_str().unwrap().to_owned();

    // Bob opens a DM with Alice — the pair is unordered, so it resolves to the
    // very same channel rather than minting a second one.
    let (status, second) = send(
        app_with_pool(pool.clone()),
        authed_post("/api/dms", bob, &create_dm_body(alice)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(second["id"].as_str().unwrap(), first_id);
}

#[tokio::test]
async fn preexisting_dm_is_reused() {
    let pool = setup_pool().await;
    let (alice, _) = seed_user(&pool, None).await;
    let (bob, _) = seed_user(&pool, None).await;
    let existing = seed_dm_channel(&pool, &[alice, bob]).await;

    let (status, body) = send(
        app_with_pool(pool.clone()),
        authed_post("/api/dms", alice, &create_dm_body(bob)),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"].as_str().unwrap(), existing.to_string());
    assert_eq!(participant_ids(&body), HashSet::from([alice, bob]));
}

#[tokio::test]
async fn dm_with_extra_member_is_not_reused() {
    // A DM channel that contains {alice, bob} plus a third member must not be
    // mistaken for alice & bob's 1:1 channel — the `count = 2` guard excludes
    // it, so the endpoint mints a fresh channel with exactly the two of them.
    let pool = setup_pool().await;
    let (alice, _) = seed_user(&pool, None).await;
    let (bob, _) = seed_user(&pool, None).await;
    let (carol, _) = seed_user(&pool, None).await;
    let group = seed_dm_channel(&pool, &[alice, bob, carol]).await;

    let (status, body) = send(
        app_with_pool(pool.clone()),
        authed_post("/api/dms", alice, &create_dm_body(bob)),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_ne!(body["id"].as_str().unwrap(), group.to_string());
    assert_eq!(participant_ids(&body), HashSet::from([alice, bob]));
}

#[tokio::test]
async fn malformed_body_is_rejected() {
    let pool = setup_pool().await;
    let (user, _) = seed_user(&pool, None).await;

    // recipient_id is required; a body missing it is unprocessable.
    let req = Request::builder()
        .method("POST")
        .uri("/api/dms")
        .header("authorization", auth_header(user))
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();

    let (status, _) = send(app_with_pool(pool.clone()), req).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}
