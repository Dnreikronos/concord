//! Integration tests for the DM-list endpoint (issue #24):
//! `GET /api/dms` and the `POST /api/dms/{id}/read` mark-read companion.
//!
//! Each test builds its own pool/runtime via `setup_pool` rather than sharing a
//! static one — see `helpers::setup_pool` for why.

mod helpers;

use std::collections::HashSet;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;
use uuid::Uuid;

use helpers::{
    app_with_pool, authed_get, authed_post, seed_dm_channel, seed_dm_read_at, seed_group_dm,
    seed_message_at, seed_user, setup_pool,
};

async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

/// The conversation in `body` (a `GET /api/dms` array) with the given id.
fn convo(body: &Value, id: Uuid) -> &Value {
    body.as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"].as_str().unwrap().parse::<Uuid>().unwrap() == id)
        .unwrap_or_else(|| panic!("conversation {id} not in {body}"))
}

fn participant_ids(convo: &Value) -> HashSet<Uuid> {
    convo["participants"]
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
        .method("GET")
        .uri("/api/dms")
        .body(Body::empty())
        .unwrap();

    let (status, _) = send(app, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn empty_when_user_has_no_dms() {
    let pool = setup_pool().await;
    let (user, _) = seed_user(&pool, None).await;

    let (status, body) = send(app_with_pool(pool.clone()), authed_get("/api/dms", user)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 0, "got: {body}");
}

#[tokio::test]
async fn lists_only_dms_the_caller_belongs_to() {
    let pool = setup_pool().await;
    let (alice, _) = seed_user(&pool, None).await;
    let (bob, _) = seed_user(&pool, None).await;
    let (carol, _) = seed_user(&pool, None).await;

    let mine_direct = seed_dm_channel(&pool, &[alice, bob]).await;
    let mine_group = seed_group_dm(&pool, Some("squad"), carol, &[alice, bob]).await;
    // A DM between bob and carol that alice has no part in.
    let not_mine = seed_dm_channel(&pool, &[bob, carol]).await;

    let (status, body) = send(app_with_pool(pool.clone()), authed_get("/api/dms", alice)).await;

    assert_eq!(status, StatusCode::OK);
    let ids: HashSet<Uuid> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().parse().unwrap())
        .collect();
    assert_eq!(ids, HashSet::from([mine_direct, mine_group]));
    assert!(!ids.contains(&not_mine), "got: {body}");
}

#[tokio::test]
async fn one_on_one_carries_participants_and_member_count() {
    let pool = setup_pool().await;
    let (alice, alice_name) = seed_user(&pool, None).await;
    let (bob, bob_name) = seed_user(&pool, None).await;
    let dm = seed_dm_channel(&pool, &[alice, bob]).await;

    let (status, body) = send(app_with_pool(pool.clone()), authed_get("/api/dms", alice)).await;
    assert_eq!(status, StatusCode::OK);

    let c = convo(&body, dm);
    assert_eq!(c["is_group"], Value::Bool(false));
    assert_eq!(c["member_count"], serde_json::json!(2));
    // A 1:1 DM carries no name or owner; both are skipped when serializing.
    assert!(c.get("name").is_none(), "got: {c}");
    assert!(c.get("owner_id").is_none(), "got: {c}");
    // No messages yet: preview omitted, nothing unread.
    assert!(c.get("last_message").is_none(), "got: {c}");
    assert_eq!(c["unread"], Value::Bool(false));

    assert_eq!(participant_ids(c), HashSet::from([alice, bob]));
    let names: HashSet<String> = c["participants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["username"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, HashSet::from([alice_name, bob_name]));
}

#[tokio::test]
async fn group_carries_name_owner_and_member_count() {
    let pool = setup_pool().await;
    let (alice, _) = seed_user(&pool, None).await;
    let (bob, _) = seed_user(&pool, None).await;
    let (carol, _) = seed_user(&pool, None).await;
    // carol owns the group; alice and bob are the other members → 3 total.
    let group = seed_group_dm(&pool, Some("the-crew"), carol, &[alice, bob]).await;

    let (status, body) = send(app_with_pool(pool.clone()), authed_get("/api/dms", alice)).await;
    assert_eq!(status, StatusCode::OK);

    let c = convo(&body, group);
    assert_eq!(c["is_group"], Value::Bool(true));
    assert_eq!(c["name"], Value::String("the-crew".into()));
    assert_eq!(
        c["owner_id"].as_str().unwrap().parse::<Uuid>().unwrap(),
        carol
    );
    assert_eq!(c["member_count"], serde_json::json!(3));
    assert_eq!(participant_ids(c), HashSet::from([alice, bob, carol]));
}

#[tokio::test]
async fn last_message_preview_is_the_newest_with_author() {
    let pool = setup_pool().await;
    let (alice, _) = seed_user(&pool, None).await;
    let (bob, bob_name) = seed_user(&pool, Some("https://cdn.example.com/b.png")).await;
    let dm = seed_dm_channel(&pool, &[alice, bob]).await;

    let base = Utc::now() - Duration::seconds(1000);
    seed_message_at(&pool, dm, Some(alice), "first", base).await;
    seed_message_at(&pool, dm, Some(bob), "latest", base + Duration::seconds(10)).await;

    let (status, body) = send(app_with_pool(pool.clone()), authed_get("/api/dms", alice)).await;
    assert_eq!(status, StatusCode::OK);

    let c = convo(&body, dm);
    let last = &c["last_message"];
    assert_eq!(last["content"], Value::String("latest".into()));
    assert_eq!(last["author"]["username"], Value::String(bob_name));
    assert_eq!(
        last["author"]["avatar_url"],
        Value::String("https://cdn.example.com/b.png".into())
    );
    assert!(last["id"].as_str().unwrap().parse::<Uuid>().is_ok());
}

#[tokio::test]
async fn last_message_with_deleted_author_omits_author() {
    let pool = setup_pool().await;
    let (alice, _) = seed_user(&pool, None).await;
    let (bob, _) = seed_user(&pool, None).await;
    let dm = seed_dm_channel(&pool, &[alice, bob]).await;
    // author_id NULL simulates a sender whose account was deleted.
    seed_message_at(
        &pool,
        dm,
        None,
        "ghost",
        Utc::now() - Duration::seconds(100),
    )
    .await;

    let (status, body) = send(app_with_pool(pool.clone()), authed_get("/api/dms", alice)).await;
    assert_eq!(status, StatusCode::OK);

    let c = convo(&body, dm);
    assert_eq!(c["last_message"]["content"], Value::String("ghost".into()));
    assert!(c["last_message"].get("author").is_none(), "got: {c}");
}

#[tokio::test]
async fn conversations_are_ordered_by_most_recent_message() {
    let pool = setup_pool().await;
    let (alice, _) = seed_user(&pool, None).await;
    let (bob, _) = seed_user(&pool, None).await;
    let (carol, _) = seed_user(&pool, None).await;
    let (dave, _) = seed_user(&pool, None).await;

    let older = seed_dm_channel(&pool, &[alice, bob]).await;
    let newer = seed_dm_channel(&pool, &[alice, carol]).await;
    let silent = seed_dm_channel(&pool, &[alice, dave]).await;

    let base = Utc::now() - Duration::seconds(1000);
    seed_message_at(&pool, older, Some(bob), "old", base).await;
    seed_message_at(
        &pool,
        newer,
        Some(carol),
        "new",
        base + Duration::seconds(100),
    )
    .await;
    // `silent` has no messages → sorts last (NULLS LAST).

    let (status, body) = send(app_with_pool(pool.clone()), authed_get("/api/dms", alice)).await;
    assert_eq!(status, StatusCode::OK);

    let order: Vec<Uuid> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().parse().unwrap())
        .collect();
    assert_eq!(order, vec![newer, older, silent], "got: {body}");
}

#[tokio::test]
async fn unread_is_true_when_another_member_posted() {
    let pool = setup_pool().await;
    let (alice, _) = seed_user(&pool, None).await;
    let (bob, _) = seed_user(&pool, None).await;
    let dm = seed_dm_channel(&pool, &[alice, bob]).await;
    seed_message_at(
        &pool,
        dm,
        Some(bob),
        "hey",
        Utc::now() - Duration::seconds(100),
    )
    .await;

    let (_, body) = send(app_with_pool(pool.clone()), authed_get("/api/dms", alice)).await;
    assert_eq!(convo(&body, dm)["unread"], Value::Bool(true));
}

#[tokio::test]
async fn unread_ignores_the_callers_own_messages() {
    let pool = setup_pool().await;
    let (alice, _) = seed_user(&pool, None).await;
    let (bob, _) = seed_user(&pool, None).await;
    let dm = seed_dm_channel(&pool, &[alice, bob]).await;
    // Only alice has posted, and she's the caller — nothing unread for her.
    seed_message_at(
        &pool,
        dm,
        Some(alice),
        "mine",
        Utc::now() - Duration::seconds(100),
    )
    .await;

    let (_, body) = send(app_with_pool(pool.clone()), authed_get("/api/dms", alice)).await;
    assert_eq!(convo(&body, dm)["unread"], Value::Bool(false));
}

#[tokio::test]
async fn unread_respects_the_last_read_high_water_mark() {
    let pool = setup_pool().await;
    let (alice, _) = seed_user(&pool, None).await;
    let (bob, _) = seed_user(&pool, None).await;
    let base = Utc::now() - Duration::seconds(1000);

    // A DM whose only message predates alice's read mark → read.
    let read = seed_dm_channel(&pool, &[alice, bob]).await;
    seed_message_at(&pool, read, Some(bob), "seen", base).await;
    seed_dm_read_at(&pool, read, alice, base + Duration::seconds(10)).await;

    // A DM whose message arrived after the read mark → still unread.
    let stale = seed_dm_channel(&pool, &[alice, bob]).await;
    seed_message_at(
        &pool,
        stale,
        Some(bob),
        "missed",
        base + Duration::seconds(20),
    )
    .await;
    seed_dm_read_at(&pool, stale, alice, base + Duration::seconds(10)).await;

    let (_, body) = send(app_with_pool(pool.clone()), authed_get("/api/dms", alice)).await;
    assert_eq!(
        convo(&body, read)["unread"],
        Value::Bool(false),
        "got: {body}"
    );
    assert_eq!(
        convo(&body, stale)["unread"],
        Value::Bool(true),
        "got: {body}"
    );
}

#[tokio::test]
async fn marking_read_clears_unread() {
    let pool = setup_pool().await;
    let (alice, _) = seed_user(&pool, None).await;
    let (bob, _) = seed_user(&pool, None).await;
    let dm = seed_dm_channel(&pool, &[alice, bob]).await;
    seed_message_at(
        &pool,
        dm,
        Some(bob),
        "hey",
        Utc::now() - Duration::seconds(60),
    )
    .await;

    let (_, before) = send(app_with_pool(pool.clone()), authed_get("/api/dms", alice)).await;
    assert_eq!(convo(&before, dm)["unread"], Value::Bool(true));

    let (status, _) = send(
        app_with_pool(pool.clone()),
        authed_post(&format!("/api/dms/{dm}/read"), alice, ""),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, after) = send(app_with_pool(pool.clone()), authed_get("/api/dms", alice)).await;
    assert_eq!(
        convo(&after, dm)["unread"],
        Value::Bool(false),
        "got: {after}"
    );
}

#[tokio::test]
async fn marking_read_as_non_member_returns_404() {
    let pool = setup_pool().await;
    let (alice, _) = seed_user(&pool, None).await;
    let (bob, _) = seed_user(&pool, None).await;
    let (outsider, _) = seed_user(&pool, None).await;
    let dm = seed_dm_channel(&pool, &[alice, bob]).await;

    let (status, _) = send(
        app_with_pool(pool.clone()),
        authed_post(&format!("/api/dms/{dm}/read"), outsider, ""),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn marking_read_on_unknown_channel_returns_404() {
    let pool = setup_pool().await;
    let (user, _) = seed_user(&pool, None).await;

    let (status, _) = send(
        app_with_pool(pool.clone()),
        authed_post(&format!("/api/dms/{}/read", Uuid::new_v4()), user, ""),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
