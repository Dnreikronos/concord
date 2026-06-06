//! Integration tests for user search (`GET /api/users/search`, issue #36) and
//! the exact-username lookup (`GET /api/users/by-username`).
//!
//! Exercises the real router against Postgres. The integration suite shares one
//! database across tests, so every case tags its seeded usernames with a unique
//! random prefix and searches for that tag — that keeps result-set assertions
//! from being polluted by users other tests seed concurrently.

mod helpers;

use std::collections::HashSet;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use helpers::{app_with_pool, authed_get, send_json, setup_pool};

/// A short, unique prefix to namespace a test's seeded usernames.
fn tag() -> String {
    Uuid::new_v4().simple().to_string()[..8].to_string()
}

/// Insert a password-auth user with an explicit username, returning its id.
async fn seed_named(pool: &PgPool, username: &str) -> Uuid {
    sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO users (username, password_hash) VALUES ($1, 'x') RETURNING id",
    )
    .bind(username)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn search_uri(query: &str) -> String {
    format!("/api/users/search?q={query}")
}

fn by_username_uri(username: &str) -> String {
    format!("/api/users/by-username?username={username}")
}

/// The usernames in a search response, in returned order.
fn usernames(body: &Value) -> Vec<String> {
    body.as_array()
        .unwrap()
        .iter()
        .map(|u| u["username"].as_str().unwrap().to_string())
        .collect()
}

/// The set of `id`s in a search response.
fn ids(body: &Value) -> HashSet<Uuid> {
    body.as_array()
        .unwrap()
        .iter()
        .map(|u| u["id"].as_str().unwrap().parse().unwrap())
        .collect()
}

#[tokio::test]
async fn unauthenticated_request_is_rejected() {
    let app = app_with_pool(setup_pool().await);
    let req = Request::builder()
        .method("GET")
        .uri(search_uri("alice"))
        .body(Body::empty())
        .unwrap();

    let (status, _) = send_json(&app, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn matches_substring_case_insensitively() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());
    let tag = tag();

    let target = seed_named(&pool, &format!("{tag}Alpha")).await;
    let _miss = seed_named(&pool, &format!("{tag}Bravo")).await;
    let caller = seed_named(&pool, &format!("{tag}caller")).await;

    // A lowercase query still matches the mixed-case username (CITEXT/ILIKE).
    let (status, body) = send_json(&app, authed_get(&search_uri(&format!("{tag}alpha")), caller)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&body), HashSet::from([target]));
}

#[tokio::test]
async fn excludes_the_caller() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());
    let tag = tag();

    let me = seed_named(&pool, &format!("{tag}me")).await;
    let other = seed_named(&pool, &format!("{tag}other")).await;

    // Searching the shared tag would match both, but the caller is filtered out.
    let (status, body) = send_json(&app, authed_get(&search_uri(&tag), me)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&body), HashSet::from([other]));
}

#[tokio::test]
async fn respects_the_limit() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());
    let tag = tag();

    for suffix in ["aaa", "bbb", "ccc"] {
        seed_named(&pool, &format!("{tag}{suffix}")).await;
    }
    let caller = seed_named(&pool, &format!("{tag}caller")).await;

    let uri = format!("{}&limit=2", search_uri(&tag));
    let (status, body) = send_json(&app, authed_get(&uri, caller)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn orders_alphabetically() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());
    let tag = tag();

    // Seeded out of order; the endpoint returns them username-sorted.
    seed_named(&pool, &format!("{tag}charlie")).await;
    seed_named(&pool, &format!("{tag}alfa")).await;
    seed_named(&pool, &format!("{tag}bravo")).await;
    let caller = seed_named(&pool, &format!("{tag}zcaller")).await;

    let (status, body) = send_json(&app, authed_get(&search_uri(&tag), caller)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        usernames(&body),
        vec![
            format!("{tag}alfa"),
            format!("{tag}bravo"),
            format!("{tag}charlie"),
        ]
    );
}

#[tokio::test]
async fn blank_query_lists_users() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());
    let tag = tag();

    // At least one selectable user besides the caller exists.
    let _other = seed_named(&pool, &format!("{tag}other")).await;
    let caller = seed_named(&pool, &format!("{tag}caller")).await;

    // A blank or whitespace-only query now lists people to pick from, rather
    // than returning nothing, so the picker is useful before you type. It stays
    // caller-excluded and limit-capped.
    for query in ["", "%20%20"] {
        let (status, body) = send_json(&app, authed_get(&search_uri(query), caller)).await;
        assert_eq!(status, StatusCode::OK, "query {query:?}");
        let list = body.as_array().unwrap();
        assert!(!list.is_empty(), "query {query:?} should list users");
        assert!(list.len() <= 20, "query {query:?} respects the default limit");
        assert!(!ids(&body).contains(&caller), "query {query:?} excludes the caller");
    }
}

#[tokio::test]
async fn like_wildcards_are_matched_literally() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());
    let tag = tag();

    // An underscore is a LIKE wildcard; escaped, it must match only the literal.
    let literal = seed_named(&pool, &format!("{tag}a_b")).await;
    let _wild = seed_named(&pool, &format!("{tag}axb")).await;
    let caller = seed_named(&pool, &format!("{tag}caller")).await;

    let (status, body) = send_json(&app, authed_get(&search_uri(&format!("{tag}a_b")), caller)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&body), HashSet::from([literal]));
}

#[tokio::test]
async fn by_username_finds_exact_match_past_the_search_cap() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());
    let tag = tag();

    // The exact target, plus a swarm of users that contain it as a substring and
    // sort *before* it alphabetically — more than the default search cap, so the
    // substring search would never surface the exact name. The exact lookup must
    // still find it (and case-insensitively).
    let target = seed_named(&pool, &format!("{tag}bob")).await;
    for i in 0..22 {
        seed_named(&pool, &format!("{tag}a{i:02}bob")).await;
    }
    let caller = seed_named(&pool, &format!("{tag}caller")).await;

    let (status, body) =
        send_json(&app, authed_get(&by_username_uri(&format!("{tag}BOB")), caller)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"].as_str().unwrap().parse::<Uuid>().unwrap(), target);
}

#[tokio::test]
async fn by_username_is_404_without_an_exact_match() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());
    let tag = tag();

    // Only a substring match exists; the exact name does not.
    let _near = seed_named(&pool, &format!("{tag}alice123")).await;
    let caller = seed_named(&pool, &format!("{tag}caller")).await;

    let (status, _) =
        send_json(&app, authed_get(&by_username_uri(&format!("{tag}alice")), caller)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn by_username_excludes_the_caller() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());
    let tag = tag();

    // The only user with this exact name is the caller, so the lookup finds no
    // one else and returns 404 rather than the caller themselves.
    let me = seed_named(&pool, &format!("{tag}solo")).await;

    let (status, _) =
        send_json(&app, authed_get(&by_username_uri(&format!("{tag}solo")), me)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
