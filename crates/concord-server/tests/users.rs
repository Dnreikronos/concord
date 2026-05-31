//! Integration tests for user search (`GET /api/users/search`, issue #36).
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
async fn blank_query_returns_empty() {
    let pool = setup_pool().await;
    let app = app_with_pool(pool.clone());
    let caller = seed_named(&pool, &format!("{}x", tag())).await;

    for query in ["", "%20%20"] {
        let (status, body) = send_json(&app, authed_get(&search_uri(query), caller)).await;
        assert_eq!(status, StatusCode::OK, "query {query:?}");
        assert!(body.as_array().unwrap().is_empty(), "query {query:?}");
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
