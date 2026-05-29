//! Integration tests for DM creation and membership.
//!
//! Covers the 1:1 DM endpoint (`POST /api/dms`, issue #21) and group-DM
//! creation plus membership (`POST /api/dms/group`, `…/members`, issue #22).
//! These exercise the real router against Postgres. Each test opens its own
//! pool (see `helpers::setup_pool`); tests that assert on stored rows keep a
//! clone of that pool and read through `concord_server::db`.

mod helpers;

use std::collections::HashSet;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

use concord_server::db;
use helpers::{
    app_with_pool, auth_header, auth_json_request, auth_request, authed_post, create_user,
    seed_dm_channel, seed_user, send_json, setup_pool, test_app,
};

const DMS: &str = "/api/dms";
const DMS_GROUP: &str = "/api/dms/group";

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
        send_json(app, auth_json_request("POST", DMS_GROUP, owner_token, &body.to_string())).await;
    assert_eq!(status, StatusCode::CREATED, "make_group failed: {dm:?}");
    dm["id"].as_str().unwrap().parse().unwrap()
}

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

// ---------------------------------------------------------------------------
// POST /api/dms — open or reuse a 1:1 DM
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// POST /api/dms/group — create a group DM
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
        send_json(&app, auth_json_request("POST", DMS_GROUP, &owner.token, &body.to_string())).await;

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
        send_json(&app, auth_json_request("POST", DMS_GROUP, &owner.token, &body.to_string())).await;

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
        send_json(&app, auth_json_request("POST", DMS_GROUP, &owner.token, &body.to_string())).await;

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
        send_json(&app, auth_json_request("POST", DMS_GROUP, &owner.token, &body.to_string())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_group_dm_rejects_only_self() {
    let app = test_app().await;
    let owner = create_user(&app).await;

    let body = json!({ "recipient_ids": [owner.id] });
    let (status, _) =
        send_json(&app, auth_json_request("POST", DMS_GROUP, &owner.token, &body.to_string())).await;
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
        send_json(&app, auth_json_request("POST", DMS_GROUP, &owner.token, &body.to_string())).await;
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
        send_json(&app, auth_json_request("POST", DMS_GROUP, &owner.token, &body.to_string())).await;

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
        send_json(&app, auth_json_request("POST", DMS_GROUP, &owner.token, &body.to_string())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_group_dm_rejects_long_name() {
    let app = test_app().await;
    let owner = create_user(&app).await;
    let a = create_user(&app).await;

    let body = json!({ "recipient_ids": [a.id], "name": "x".repeat(101) });
    let (status, _) =
        send_json(&app, auth_json_request("POST", DMS_GROUP, &owner.token, &body.to_string())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_group_dm_requires_auth() {
    let app = test_app().await;
    let body = json!({ "recipient_ids": [Uuid::new_v4()] });
    let req = Request::builder()
        .method("POST")
        .uri(DMS_GROUP)
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
