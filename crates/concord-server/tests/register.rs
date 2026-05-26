mod helpers;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use helpers::{random_email, random_username, register_request, test_app};

async fn response_json(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

// 1 — Happy path
#[tokio::test]
async fn register_valid_user() {
    let app = test_app().await;
    let body = json!({
        "username": random_username(),
        "email": random_email(),
        "password": "securepass1"
    });
    let (status, body) = response_json(app, register_request(&body.to_string())).await;

    assert_eq!(status, StatusCode::CREATED);
    assert!(body.get("id").is_some());
    assert!(body.get("username").is_some());
    assert!(body.get("email").is_some());
    assert!(body.get("status").is_some());
    assert!(body.get("created_at").is_some());
    assert!(body.get("updated_at").is_some());
    assert!(body.get("password_hash").is_none());
}

// 2 — Short username
#[tokio::test]
async fn register_short_username() {
    let app = test_app().await;
    let body = json!({
        "username": "ab",
        "email": random_email(),
        "password": "securepass1"
    });
    let (status, body) = response_json(app, register_request(&body.to_string())).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let err = body["error"].as_str().unwrap();
    assert!(err.contains("at least 3 characters"), "got: {err}");
}

// 3 — Long username
#[tokio::test]
async fn register_long_username() {
    let app = test_app().await;
    let body = json!({
        "username": "a".repeat(33),
        "email": random_email(),
        "password": "securepass1"
    });
    let (status, body) = response_json(app, register_request(&body.to_string())).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let err = body["error"].as_str().unwrap();
    assert!(err.contains("at most 32 characters"), "got: {err}");
}

// 4 — Username with spaces
#[tokio::test]
async fn register_username_with_spaces() {
    let app = test_app().await;
    let body = json!({
        "username": "has space",
        "email": random_email(),
        "password": "securepass1"
    });
    let (status, body) = response_json(app, register_request(&body.to_string())).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let err = body["error"].as_str().unwrap();
    assert!(err.contains("invalid characters"), "got: {err}");
}

// 5 — Invalid email (no @)
#[tokio::test]
async fn register_invalid_email() {
    let app = test_app().await;
    let body = json!({
        "username": random_username(),
        "email": "notanemail",
        "password": "securepass1"
    });
    let (status, body) = response_json(app, register_request(&body.to_string())).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let err = body["error"].as_str().unwrap();
    assert!(err.contains("invalid email"), "got: {err}");
}

// 6 — Short password
#[tokio::test]
async fn register_short_password() {
    let app = test_app().await;
    let body = json!({
        "username": random_username(),
        "email": random_email(),
        "password": "1234567"
    });
    let (status, body) = response_json(app, register_request(&body.to_string())).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let err = body["error"].as_str().unwrap();
    assert!(err.contains("at least 8 characters"), "got: {err}");
}

// 7 — Long password
#[tokio::test]
async fn register_long_password() {
    let app = test_app().await;
    let body = json!({
        "username": random_username(),
        "email": random_email(),
        "password": "a".repeat(129)
    });
    let (status, body) = response_json(app, register_request(&body.to_string())).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let err = body["error"].as_str().unwrap();
    assert!(err.contains("at most 128 characters"), "got: {err}");
}

// 8 — Duplicate username
#[tokio::test]
async fn register_duplicate_username() {
    let app = test_app().await;
    let username = random_username();
    let body1 = json!({
        "username": username,
        "email": random_email(),
        "password": "securepass1"
    });
    let body2 = json!({
        "username": username,
        "email": random_email(),
        "password": "securepass1"
    });

    let (s1, _) = response_json(app.clone(), register_request(&body1.to_string())).await;
    assert_eq!(s1, StatusCode::CREATED);

    let (s2, body) = response_json(app, register_request(&body2.to_string())).await;
    assert_eq!(s2, StatusCode::CONFLICT);
    let err = body["error"].as_str().unwrap();
    assert!(err.contains("username already exists"), "got: {err}");
}

// 9 — Duplicate email
#[tokio::test]
async fn register_duplicate_email() {
    let app = test_app().await;
    let email = random_email();
    let body1 = json!({
        "username": random_username(),
        "email": email,
        "password": "securepass1"
    });
    let body2 = json!({
        "username": random_username(),
        "email": email,
        "password": "securepass1"
    });

    let (s1, _) = response_json(app.clone(), register_request(&body1.to_string())).await;
    assert_eq!(s1, StatusCode::CREATED);

    let (s2, body) = response_json(app, register_request(&body2.to_string())).await;
    assert_eq!(s2, StatusCode::CONFLICT);
    let err = body["error"].as_str().unwrap();
    assert!(err.contains("email already exists"), "got: {err}");
}

// 10 — Case-insensitive duplicate username
#[tokio::test]
async fn register_case_insensitive_duplicate_username() {
    let app = test_app().await;
    let base = random_username();
    let body1 = json!({
        "username": base.to_lowercase(),
        "email": random_email(),
        "password": "securepass1"
    });
    let body2 = json!({
        "username": base.to_uppercase(),
        "email": random_email(),
        "password": "securepass1"
    });

    let (s1, _) = response_json(app.clone(), register_request(&body1.to_string())).await;
    assert_eq!(s1, StatusCode::CREATED);

    let (s2, _) = response_json(app, register_request(&body2.to_string())).await;
    assert_eq!(s2, StatusCode::CONFLICT);
}

// 11 — Case-insensitive duplicate email
#[tokio::test]
async fn register_case_insensitive_duplicate_email() {
    let app = test_app().await;
    let email = random_email();
    let body1 = json!({
        "username": random_username(),
        "email": email.to_lowercase(),
        "password": "securepass1"
    });
    let body2 = json!({
        "username": random_username(),
        "email": email.to_uppercase(),
        "password": "securepass1"
    });

    let (s1, _) = response_json(app.clone(), register_request(&body1.to_string())).await;
    assert_eq!(s1, StatusCode::CREATED);

    let (s2, _) = response_json(app, register_request(&body2.to_string())).await;
    assert_eq!(s2, StatusCode::CONFLICT);
}

// 12 — Missing fields
#[tokio::test]
async fn register_missing_fields() {
    let app = test_app().await;
    let req = register_request(r#"{"username": "alice"}"#);
    let (status, _) = response_json(app, req).await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

// 13 — Missing Content-Type
#[tokio::test]
async fn register_missing_content_type() {
    let app = test_app().await;
    let body = json!({
        "username": random_username(),
        "email": random_email(),
        "password": "securepass1"
    });
    let req = Request::builder()
        .method("POST")
        .uri("/api/auth/register")
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, _) = response_json(app, req).await;

    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

// 14 — Extra fields ignored
#[tokio::test]
async fn register_extra_fields_ignored() {
    let app = test_app().await;
    let body = json!({
        "username": random_username(),
        "email": random_email(),
        "password": "securepass1",
        "role": "admin",
        "extra": true
    });
    let (status, _) = response_json(app, register_request(&body.to_string())).await;

    assert_eq!(status, StatusCode::CREATED);
}

// 15 — Trimmed whitespace on username/email
#[tokio::test]
async fn register_trims_whitespace() {
    let app = test_app().await;
    let username = random_username();
    let email = random_email();
    let body = json!({
        "username": format!("  {username}  "),
        "email": format!("  {email}  "),
        "password": "securepass1"
    });
    let (status, body) = response_json(app, register_request(&body.to_string())).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["username"].as_str().unwrap(), username);
    assert_eq!(body["email"].as_str().unwrap(), email);
}

// 16 — Whitespace in email local part
#[tokio::test]
async fn register_whitespace_in_email_local_part() {
    let app = test_app().await;
    let body = json!({
        "username": random_username(),
        "email": "ali ce@example.com",
        "password": "securepass1"
    });
    let (status, body) = response_json(app, register_request(&body.to_string())).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let err = body["error"].as_str().unwrap();
    assert!(err.contains("invalid email"), "got: {err}");
}
