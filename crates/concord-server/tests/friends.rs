//! Integration tests for the friends system (`/api/friends`).
//!
//! Exercises the real router against Postgres. The suite shares one database
//! across tests, but every case seeds its own fresh users and only ever asserts
//! on those users' own friend/request lists, so concurrent tests don't pollute
//! each other.

mod helpers;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use uuid::Uuid;

use helpers::{app_with_pool, auth_header, authed_get, authed_post, seed_user, send_json, setup_pool};

fn send_request_body(user_id: Uuid) -> String {
    json!({ "user_id": user_id }).to_string()
}

/// A `DELETE uri` request authenticated as `user_id`.
fn authed_delete(uri: &str, user_id: Uuid) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(uri)
        .header("authorization", auth_header(user_id))
        .body(Body::empty())
        .unwrap()
}

/// The `user.id`s in a friends-list or request-array response, as a set.
fn user_ids(arr: &Value) -> Vec<Uuid> {
    arr.as_array()
        .unwrap()
        .iter()
        .map(|e| e["user"]["id"].as_str().unwrap().parse().unwrap())
        .collect()
}

/// Fetch `user`'s pending requests (the `{incoming, outgoing}` object).
async fn get_requests(app: &axum::Router, user: Uuid) -> Value {
    let (status, body) = send_json(app, authed_get("/api/friends/requests", user)).await;
    assert_eq!(status, StatusCode::OK);
    body
}

/// Fetch `user`'s accepted friends.
async fn get_friends(app: &axum::Router, user: Uuid) -> Value {
    let (status, body) = send_json(app, authed_get("/api/friends", user)).await;
    assert_eq!(status, StatusCode::OK);
    body
}

#[tokio::test]
async fn unauthenticated_request_is_rejected() {
    let app = app_with_pool(setup_pool().await);
    let req = Request::builder()
        .method("GET")
        .uri("/api/friends")
        .body(Body::empty())
        .unwrap();

    let (status, _) = send_json(&app, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn send_request_appears_in_both_users_lists() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());
    let (a, _) = seed_user(&pool, None).await;
    let (b, _) = seed_user(&pool, None).await;

    let (status, body) =
        send_json(&app, authed_post("/api/friends/requests", a, &send_request_body(b))).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["outcome"], "requested");
    assert_eq!(body["user"]["id"].as_str().unwrap().parse::<Uuid>().unwrap(), b);

    // Sender sees it outgoing, recipient sees it incoming.
    let a_reqs = get_requests(&app, a).await;
    assert_eq!(user_ids(&a_reqs["outgoing"]), vec![b]);
    assert!(a_reqs["incoming"].as_array().unwrap().is_empty());

    let b_reqs = get_requests(&app, b).await;
    assert_eq!(user_ids(&b_reqs["incoming"]), vec![a]);
    assert!(b_reqs["outgoing"].as_array().unwrap().is_empty());

    // Pending is not yet a friendship.
    assert!(get_friends(&app, a).await.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn cannot_friend_self() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());
    let (a, _) = seed_user(&pool, None).await;

    let (status, _) =
        send_json(&app, authed_post("/api/friends/requests", a, &send_request_body(a))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn request_to_unknown_user_is_not_found() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());
    let (a, _) = seed_user(&pool, None).await;

    let (status, _) = send_json(
        &app,
        authed_post("/api/friends/requests", a, &send_request_body(Uuid::new_v4())),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn duplicate_request_conflicts() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());
    let (a, _) = seed_user(&pool, None).await;
    let (b, _) = seed_user(&pool, None).await;

    let (status, _) =
        send_json(&app, authed_post("/api/friends/requests", a, &send_request_body(b))).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _) =
        send_json(&app, authed_post("/api/friends/requests", a, &send_request_body(b))).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn reverse_request_auto_accepts() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());
    let (a, _) = seed_user(&pool, None).await;
    let (b, _) = seed_user(&pool, None).await;

    let (status, _) =
        send_json(&app, authed_post("/api/friends/requests", a, &send_request_body(b))).await;
    assert_eq!(status, StatusCode::CREATED);

    // b requesting a back accepts the pending one instead of making a second.
    let (status, body) =
        send_json(&app, authed_post("/api/friends/requests", b, &send_request_body(a))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["outcome"], "accepted");

    assert_eq!(user_ids(&get_friends(&app, a).await), vec![b]);
    assert_eq!(user_ids(&get_friends(&app, b).await), vec![a]);

    // No pending requests linger on either side.
    let a_reqs = get_requests(&app, a).await;
    assert!(a_reqs["incoming"].as_array().unwrap().is_empty());
    assert!(a_reqs["outgoing"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn accept_makes_them_friends() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());
    let (a, _) = seed_user(&pool, None).await;
    let (b, _) = seed_user(&pool, None).await;

    send_json(&app, authed_post("/api/friends/requests", a, &send_request_body(b))).await;

    let b_reqs = get_requests(&app, b).await;
    let request_id = b_reqs["incoming"][0]["id"].as_str().unwrap();

    let (status, body) = send_json(
        &app,
        authed_post(&format!("/api/friends/requests/{request_id}/accept"), b, ""),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["user"]["id"].as_str().unwrap().parse::<Uuid>().unwrap(), a);

    assert_eq!(user_ids(&get_friends(&app, a).await), vec![b]);
    assert_eq!(user_ids(&get_friends(&app, b).await), vec![a]);
    assert!(get_requests(&app, b).await["incoming"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn only_addressee_can_accept() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());
    let (a, _) = seed_user(&pool, None).await;
    let (b, _) = seed_user(&pool, None).await;

    send_json(&app, authed_post("/api/friends/requests", a, &send_request_body(b))).await;
    let a_reqs = get_requests(&app, a).await;
    let request_id = a_reqs["outgoing"][0]["id"].as_str().unwrap();

    // The requester (a) cannot accept their own outgoing request.
    let (status, _) = send_json(
        &app,
        authed_post(&format!("/api/friends/requests/{request_id}/accept"), a, ""),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reject_removes_the_request() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());
    let (a, _) = seed_user(&pool, None).await;
    let (b, _) = seed_user(&pool, None).await;

    send_json(&app, authed_post("/api/friends/requests", a, &send_request_body(b))).await;
    let request_id = get_requests(&app, b).await["incoming"][0]["id"].as_str().unwrap().to_owned();

    let (status, _) =
        send_json(&app, authed_delete(&format!("/api/friends/requests/{request_id}"), b)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    assert!(get_requests(&app, a).await["outgoing"].as_array().unwrap().is_empty());
    assert!(get_requests(&app, b).await["incoming"].as_array().unwrap().is_empty());
    assert!(get_friends(&app, a).await.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn requester_can_cancel_the_request() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());
    let (a, _) = seed_user(&pool, None).await;
    let (b, _) = seed_user(&pool, None).await;

    send_json(&app, authed_post("/api/friends/requests", a, &send_request_body(b))).await;
    let request_id = get_requests(&app, a).await["outgoing"][0]["id"].as_str().unwrap().to_owned();

    let (status, _) =
        send_json(&app, authed_delete(&format!("/api/friends/requests/{request_id}"), a)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    assert!(get_requests(&app, b).await["incoming"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn remove_friend_unfriends_both() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());
    let (a, _) = seed_user(&pool, None).await;
    let (b, _) = seed_user(&pool, None).await;

    // Establish the friendship via the reverse-accept shortcut.
    send_json(&app, authed_post("/api/friends/requests", a, &send_request_body(b))).await;
    send_json(&app, authed_post("/api/friends/requests", b, &send_request_body(a))).await;
    assert_eq!(user_ids(&get_friends(&app, a).await), vec![b]);

    let (status, _) = send_json(&app, authed_delete(&format!("/api/friends/{b}"), a)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    assert!(get_friends(&app, a).await.as_array().unwrap().is_empty());
    assert!(get_friends(&app, b).await.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn remove_non_friend_is_not_found() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());
    let (a, _) = seed_user(&pool, None).await;
    let (b, _) = seed_user(&pool, None).await;

    let (status, _) = send_json(&app, authed_delete(&format!("/api/friends/{b}"), a)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn request_between_existing_friends_conflicts() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());
    let (a, _) = seed_user(&pool, None).await;
    let (b, _) = seed_user(&pool, None).await;

    send_json(&app, authed_post("/api/friends/requests", a, &send_request_body(b))).await;
    send_json(&app, authed_post("/api/friends/requests", b, &send_request_body(a))).await;

    // Already friends: a fresh request is a conflict.
    let (status, _) =
        send_json(&app, authed_post("/api/friends/requests", a, &send_request_body(b))).await;
    assert_eq!(status, StatusCode::CONFLICT);
}
