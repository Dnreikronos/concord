//! Integration tests for group-DM creation and membership (issue #22).
//!
//! These exercise the real router against Postgres. Each test opens its own
//! pool (see `helpers::setup_pool`); tests that assert on stored rows keep a
//! clone of that pool and read through `concord_server::db`.

mod helpers;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::json;
use uuid::Uuid;

use concord_server::db;
use helpers::{
    app_with_pool, auth_json_request, auth_request, create_user, send_json, setup_pool, test_app,
};

const DMS: &str = "/api/dms";

fn members_uri(group: Uuid) -> String {
    format!("{DMS}/{group}/members")
}

fn member_uri(group: Uuid, user: Uuid) -> String {
    format!("{DMS}/{group}/members/{user}")
}

/// Create a group DM owned by `owner_token` with the given recipients and
/// return its id, asserting a 201.
async fn make_group(app: &Router, owner_token: &str, recipients: &[Uuid]) -> Uuid {
    let body = json!({ "recipient_ids": recipients });
    let (status, dm) =
        send_json(app, auth_json_request("POST", DMS, owner_token, &body.to_string())).await;
    assert_eq!(status, StatusCode::CREATED, "make_group failed: {dm:?}");
    dm["id"].as_str().unwrap().parse().unwrap()
}

// ---------------------------------------------------------------------------
// POST /api/dms — create
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_group_dm_happy() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());

    let owner = create_user(&app).await;
    let a = create_user(&app).await;
    let b = create_user(&app).await;

    let body = json!({ "recipient_ids": [a.id, b.id] });
    let (status, dm) =
        send_json(&app, auth_json_request("POST", DMS, &owner.token, &body.to_string())).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(dm["is_group"].as_bool(), Some(true));
    assert_eq!(dm["owner_id"].as_str().unwrap(), owner.id.to_string());

    let id: Uuid = dm["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(db::dm_member_count(&pool, id).await.unwrap(), 3);
    assert!(db::is_dm_member(&pool, id, owner.id).await.unwrap());
    assert!(db::is_dm_member(&pool, id, a.id).await.unwrap());
    assert!(db::is_dm_member(&pool, id, b.id).await.unwrap());
}

#[tokio::test]
async fn create_group_dm_trims_and_keeps_name() {
    let app = test_app().await;
    let owner = create_user(&app).await;
    let a = create_user(&app).await;

    let body = json!({ "recipient_ids": [a.id], "name": "  weekend trip  " });
    let (status, dm) =
        send_json(&app, auth_json_request("POST", DMS, &owner.token, &body.to_string())).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(dm["name"].as_str().unwrap(), "weekend trip");
}

#[tokio::test]
async fn create_group_dm_dedups_and_strips_self() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());

    let owner = create_user(&app).await;
    let a = create_user(&app).await;

    // Caller lists themselves and duplicates a recipient; only {a} is effective.
    let body = json!({ "recipient_ids": [owner.id, a.id, a.id] });
    let (status, dm) =
        send_json(&app, auth_json_request("POST", DMS, &owner.token, &body.to_string())).await;

    assert_eq!(status, StatusCode::CREATED);
    let id: Uuid = dm["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(db::dm_member_count(&pool, id).await.unwrap(), 2);
}

#[tokio::test]
async fn create_group_dm_requires_another_participant() {
    let app = test_app().await;
    let owner = create_user(&app).await;

    let body = json!({ "recipient_ids": [] });
    let (status, _) =
        send_json(&app, auth_json_request("POST", DMS, &owner.token, &body.to_string())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_group_dm_rejects_only_self() {
    let app = test_app().await;
    let owner = create_user(&app).await;

    let body = json!({ "recipient_ids": [owner.id] });
    let (status, _) =
        send_json(&app, auth_json_request("POST", DMS, &owner.token, &body.to_string())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_group_dm_rejects_too_many() {
    let app = test_app().await;
    let owner = create_user(&app).await;

    // 10 recipients + creator = 11; the size check fires before any DB lookup,
    // so unregistered ids are fine here.
    let recipients: Vec<Uuid> = (0..10).map(|_| Uuid::new_v4()).collect();
    let body = json!({ "recipient_ids": recipients });
    let (status, _) =
        send_json(&app, auth_json_request("POST", DMS, &owner.token, &body.to_string())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_group_dm_allows_ten_participants() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());

    let owner = create_user(&app).await;
    let mut ids = Vec::new();
    for _ in 0..9 {
        ids.push(create_user(&app).await.id);
    }

    let body = json!({ "recipient_ids": ids });
    let (status, dm) =
        send_json(&app, auth_json_request("POST", DMS, &owner.token, &body.to_string())).await;

    assert_eq!(status, StatusCode::CREATED);
    let id: Uuid = dm["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(db::dm_member_count(&pool, id).await.unwrap(), 10);
}

#[tokio::test]
async fn create_group_dm_rejects_unknown_recipient() {
    let app = test_app().await;
    let owner = create_user(&app).await;
    let real = create_user(&app).await;

    // Total of 3 keeps the size check happy; the bogus id fails existence.
    let body = json!({ "recipient_ids": [real.id, Uuid::new_v4()] });
    let (status, _) =
        send_json(&app, auth_json_request("POST", DMS, &owner.token, &body.to_string())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_group_dm_rejects_long_name() {
    let app = test_app().await;
    let owner = create_user(&app).await;
    let a = create_user(&app).await;

    let body = json!({ "recipient_ids": [a.id], "name": "x".repeat(101) });
    let (status, _) =
        send_json(&app, auth_json_request("POST", DMS, &owner.token, &body.to_string())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_group_dm_requires_auth() {
    let app = test_app().await;
    let body = json!({ "recipient_ids": [Uuid::new_v4()] });
    let req = Request::builder()
        .method("POST")
        .uri(DMS)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, _) = send_json(&app, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// POST /api/dms/{id}/members — add
// ---------------------------------------------------------------------------

#[tokio::test]
async fn add_member_happy() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());

    let owner = create_user(&app).await;
    let a = create_user(&app).await;
    let newcomer = create_user(&app).await;
    let gid = make_group(&app, &owner.token, &[a.id]).await;

    let body = json!({ "user_id": newcomer.id });
    let (status, _) =
        send_json(&app, auth_json_request("POST", &members_uri(gid), &owner.token, &body.to_string()))
            .await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(db::dm_member_count(&pool, gid).await.unwrap(), 3);
    assert!(db::is_dm_member(&pool, gid, newcomer.id).await.unwrap());
}

#[tokio::test]
async fn add_member_allowed_for_any_member() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());

    let owner = create_user(&app).await;
    let member = create_user(&app).await;
    let newcomer = create_user(&app).await;
    let gid = make_group(&app, &owner.token, &[member.id]).await;

    // A non-owner member may add others; only removal is owner-gated.
    let body = json!({ "user_id": newcomer.id });
    let (status, _) = send_json(
        &app,
        auth_json_request("POST", &members_uri(gid), &member.token, &body.to_string()),
    )
    .await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(db::is_dm_member(&pool, gid, newcomer.id).await.unwrap());
}

#[tokio::test]
async fn add_member_already_present_conflicts() {
    let app = test_app().await;
    let owner = create_user(&app).await;
    let a = create_user(&app).await;
    let gid = make_group(&app, &owner.token, &[a.id]).await;

    let body = json!({ "user_id": a.id });
    let (status, _) =
        send_json(&app, auth_json_request("POST", &members_uri(gid), &owner.token, &body.to_string()))
            .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn add_member_unknown_user() {
    let app = test_app().await;
    let owner = create_user(&app).await;
    let a = create_user(&app).await;
    let gid = make_group(&app, &owner.token, &[a.id]).await;

    let body = json!({ "user_id": Uuid::new_v4() });
    let (status, _) =
        send_json(&app, auth_json_request("POST", &members_uri(gid), &owner.token, &body.to_string()))
            .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn add_member_unknown_channel() {
    let app = test_app().await;
    let owner = create_user(&app).await;
    let stranger = create_user(&app).await;

    let body = json!({ "user_id": stranger.id });
    let (status, _) = send_json(
        &app,
        auth_json_request("POST", &members_uri(Uuid::new_v4()), &owner.token, &body.to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn add_member_by_non_member_is_hidden() {
    let app = test_app().await;
    let owner = create_user(&app).await;
    let a = create_user(&app).await;
    let outsider = create_user(&app).await;
    let target = create_user(&app).await;
    let gid = make_group(&app, &owner.token, &[a.id]).await;

    let body = json!({ "user_id": target.id });
    let (status, _) = send_json(
        &app,
        auth_json_request("POST", &members_uri(gid), &outsider.token, &body.to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn add_member_rejected_when_full() {
    let app = test_app().await;
    let owner = create_user(&app).await;
    let mut ids = Vec::new();
    for _ in 0..9 {
        ids.push(create_user(&app).await.id);
    }
    let gid = make_group(&app, &owner.token, &ids).await; // 10 participants

    let extra = create_user(&app).await;
    let body = json!({ "user_id": extra.id });
    let (status, _) =
        send_json(&app, auth_json_request("POST", &members_uri(gid), &owner.token, &body.to_string()))
            .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn add_member_requires_auth() {
    let app = test_app().await;
    let body = json!({ "user_id": Uuid::new_v4() });
    let req = Request::builder()
        .method("POST")
        .uri(members_uri(Uuid::new_v4()))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, _) = send_json(&app, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// DELETE /api/dms/{id}/members/{user_id} — remove / leave
// ---------------------------------------------------------------------------

#[tokio::test]
async fn leave_group_removes_self() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());

    let owner = create_user(&app).await;
    let a = create_user(&app).await;
    let b = create_user(&app).await;
    let gid = make_group(&app, &owner.token, &[a.id, b.id]).await;

    let (status, _) =
        send_json(&app, auth_request("DELETE", &member_uri(gid, a.id), &a.token)).await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!db::is_dm_member(&pool, gid, a.id).await.unwrap());
    assert_eq!(db::dm_member_count(&pool, gid).await.unwrap(), 2);
}

#[tokio::test]
async fn owner_removes_member() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());

    let owner = create_user(&app).await;
    let a = create_user(&app).await;
    let b = create_user(&app).await;
    let gid = make_group(&app, &owner.token, &[a.id, b.id]).await;

    let (status, _) =
        send_json(&app, auth_request("DELETE", &member_uri(gid, a.id), &owner.token)).await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!db::is_dm_member(&pool, gid, a.id).await.unwrap());
}

#[tokio::test]
async fn non_owner_cannot_remove_other() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());

    let owner = create_user(&app).await;
    let a = create_user(&app).await;
    let b = create_user(&app).await;
    let gid = make_group(&app, &owner.token, &[a.id, b.id]).await;

    // a is a member but not the owner; removing b must be refused.
    let (status, _) =
        send_json(&app, auth_request("DELETE", &member_uri(gid, b.id), &a.token)).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(db::is_dm_member(&pool, gid, b.id).await.unwrap());
}

#[tokio::test]
async fn remove_target_not_member() {
    let app = test_app().await;
    let owner = create_user(&app).await;
    let a = create_user(&app).await;
    let outsider = create_user(&app).await;
    let gid = make_group(&app, &owner.token, &[a.id]).await;

    let (status, _) = send_json(
        &app,
        auth_request("DELETE", &member_uri(gid, outsider.id), &owner.token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn remove_from_unknown_channel() {
    let app = test_app().await;
    let owner = create_user(&app).await;

    let (status, _) = send_json(
        &app,
        auth_request("DELETE", &member_uri(Uuid::new_v4(), owner.id), &owner.token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn remove_by_non_member_is_hidden() {
    let app = test_app().await;
    let owner = create_user(&app).await;
    let a = create_user(&app).await;
    let outsider = create_user(&app).await;
    let gid = make_group(&app, &owner.token, &[a.id]).await;

    let (status, _) =
        send_json(&app, auth_request("DELETE", &member_uri(gid, a.id), &outsider.token)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn owner_leaving_transfers_ownership() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());

    let owner = create_user(&app).await;
    let a = create_user(&app).await;
    let b = create_user(&app).await;
    let gid = make_group(&app, &owner.token, &[a.id, b.id]).await;

    let (status, _) =
        send_json(&app, auth_request("DELETE", &member_uri(gid, owner.id), &owner.token)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let dm = db::get_group_dm(&pool, gid).await.unwrap().expect("channel still exists");
    let new_owner = dm.owner_id.expect("ownership should transfer, not drop to null");
    assert_ne!(new_owner, owner.id);
    assert!(db::is_dm_member(&pool, gid, new_owner).await.unwrap());
}

#[tokio::test]
async fn last_member_leaving_deletes_channel() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());

    let owner = create_user(&app).await;
    let a = create_user(&app).await;
    let gid = make_group(&app, &owner.token, &[a.id]).await;

    // Owner leaves: ownership passes to a, one member remains.
    let (s1, _) =
        send_json(&app, auth_request("DELETE", &member_uri(gid, owner.id), &owner.token)).await;
    assert_eq!(s1, StatusCode::NO_CONTENT);

    // a leaves: the group is now empty and the channel is cleaned up.
    let (s2, _) = send_json(&app, auth_request("DELETE", &member_uri(gid, a.id), &a.token)).await;
    assert_eq!(s2, StatusCode::NO_CONTENT);

    assert!(db::get_group_dm(&pool, gid).await.unwrap().is_none());
    assert_eq!(db::dm_member_count(&pool, gid).await.unwrap(), 0);
}

#[tokio::test]
async fn remove_requires_auth() {
    let app = test_app().await;
    let req = Request::builder()
        .method("DELETE")
        .uri(member_uri(Uuid::new_v4(), Uuid::new_v4()))
        .body(Body::empty())
        .unwrap();
    let (status, _) = send_json(&app, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
