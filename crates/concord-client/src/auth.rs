//! Authentication against the server's `/api/auth/*` REST endpoints.
//!
//! The module is split in two: the wire types, the OAuth URL builder, and the
//! [`Session`] / [`AuthError`] values are always available (and unit-tested),
//! while the actual HTTP round-trips live behind the `gui` feature because
//! `reqwest` is only pulled in for the desktop client.
//!
//! Tokens are returned in a [`Session`] and held in memory by the caller;
//! persistent storage (keyring) is a later concern.

use concord_shared::types::{OAuthProvider, User};
use serde::Deserialize;
#[cfg(feature = "gui")]
use serde::Serialize;

/// Default base URL of the Concord HTTP API, overridable via `CONCORD_API_URL`.
/// Matches the server's default bind port (8080).
const DEFAULT_API_URL: &str = "http://127.0.0.1:8080";

/// The HTTP API base URL, taken from `CONCORD_API_URL` or the default.
pub fn api_base_url() -> String {
    std::env::var("CONCORD_API_URL").unwrap_or_else(|_| DEFAULT_API_URL.into())
}

/// Build the URL that begins the browser-based OAuth flow for `provider`.
///
/// The desktop client opens this in the system browser; the server handles the
/// redirect and provider callback.
pub fn oauth_url(base_url: &str, provider: OAuthProvider) -> String {
    format!(
        "{}/api/auth/oauth/{}",
        base_url.trim_end_matches('/'),
        provider
    )
}

/// A successful authentication: the issued tokens plus the resolved user.
#[derive(Debug, Clone)]
pub struct Session {
    pub access_token: String,
    pub refresh_token: String,
    pub user: User,
}

/// Why an authentication attempt failed.
#[derive(Debug, Clone)]
pub enum AuthError {
    /// Inputs failed a client-side check before any request was sent.
    Validation(String),
    /// The request never completed (DNS, connection refused, timeout, ...).
    Network(String),
    /// The server rejected the request and returned an error message.
    Server(String),
    /// The server answered, but not in a shape we could understand.
    Unexpected(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Validation(m) => write!(f, "{m}"),
            Self::Network(m) => write!(f, "could not reach the server: {m}"),
            Self::Server(m) => write!(f, "{m}"),
            Self::Unexpected(m) => write!(f, "unexpected response: {m}"),
        }
    }
}

impl std::error::Error for AuthError {}

// ---------------------------------------------------------------------------
// Wire bodies (mirror the server's `routes::auth` request / response shapes).
// ---------------------------------------------------------------------------

// Request bodies are only built by the `gui`-only HTTP calls below.
#[cfg(feature = "gui")]
#[derive(Serialize)]
struct LoginBody<'a> {
    email: &'a str,
    password: &'a str,
}

#[cfg(feature = "gui")]
#[derive(Serialize)]
struct RegisterBody<'a> {
    username: &'a str,
    email: &'a str,
    password: &'a str,
}

/// Server's `LoginResponse`: access + refresh tokens and the user.
#[derive(Deserialize)]
struct LoginResponse {
    access_token: String,
    refresh_token: String,
    user: User,
}

/// Server's error envelope: `{ "error": "..." }`.
#[derive(Deserialize)]
struct ErrorBody {
    error: String,
}

// ---------------------------------------------------------------------------
// HTTP round-trips. `gui`-only — these are the sole users of `reqwest`.
// ---------------------------------------------------------------------------

/// Shared HTTP client with a request timeout, so a stalled server can't leave
/// the submit flow waiting forever. Built once and reused to keep the
/// connection pool and TLS state warm across calls.
#[cfg(feature = "gui")]
fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// Log in with email + password, returning the issued [`Session`].
#[cfg(feature = "gui")]
pub async fn login(base_url: &str, email: &str, password: &str) -> Result<Session, AuthError> {
    let base = base_url.trim_end_matches('/');
    let resp = http_client()
        .post(format!("{base}/api/auth/login"))
        .json(&LoginBody { email, password })
        .send()
        .await
        .map_err(|e| AuthError::Network(e.to_string()))?;
    session_from_response(resp).await
}

/// Register an account, then log in to obtain a [`Session`].
///
/// The server's `/register` endpoint creates the user but issues no tokens, so
/// a successful registration is immediately followed by a login with the same
/// credentials.
#[cfg(feature = "gui")]
pub async fn register(
    base_url: &str,
    username: &str,
    email: &str,
    password: &str,
) -> Result<Session, AuthError> {
    let base = base_url.trim_end_matches('/');
    let resp = http_client()
        .post(format!("{base}/api/auth/register"))
        .json(&RegisterBody {
            username,
            email,
            password,
        })
        .send()
        .await
        .map_err(|e| AuthError::Network(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(server_error(resp).await);
    }
    login(base, email, password).await
}

/// Turn a login/register response into a [`Session`], or extract the error.
#[cfg(feature = "gui")]
async fn session_from_response(resp: reqwest::Response) -> Result<Session, AuthError> {
    if !resp.status().is_success() {
        return Err(server_error(resp).await);
    }
    let body: LoginResponse = resp
        .json()
        .await
        .map_err(|e| AuthError::Unexpected(e.to_string()))?;
    Ok(Session {
        access_token: body.access_token,
        refresh_token: body.refresh_token,
        user: body.user,
    })
}

/// Read the server's error message off a non-2xx response, falling back to the
/// status code if the body isn't the expected envelope.
#[cfg(feature = "gui")]
async fn server_error(resp: reqwest::Response) -> AuthError {
    let status = resp.status();
    match resp.json::<ErrorBody>().await {
        Ok(body) => AuthError::Server(body.error),
        Err(_) => AuthError::Server(format!("request failed ({status})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_url_builds_provider_path() {
        assert_eq!(
            oauth_url("http://localhost:8080", OAuthProvider::Github),
            "http://localhost:8080/api/auth/oauth/github"
        );
        assert_eq!(
            oauth_url("http://localhost:8080", OAuthProvider::Google),
            "http://localhost:8080/api/auth/oauth/google"
        );
    }

    #[test]
    fn oauth_url_trims_trailing_slash() {
        assert_eq!(
            oauth_url("http://localhost:8080/", OAuthProvider::Github),
            "http://localhost:8080/api/auth/oauth/github"
        );
    }

    #[test]
    fn login_response_parses_with_optional_user_fields_present() {
        let json = r#"{
            "access_token": "acc",
            "refresh_token": "ref",
            "user": {
                "id": "00000000-0000-0000-0000-000000000001",
                "username": "alice",
                "email": "alice@example.com",
                "avatar_url": "http://img/a.png",
                "status": "online",
                "oauth_provider": "github",
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z"
            }
        }"#;
        let resp: LoginResponse = serde_json::from_str(json).expect("parse");
        assert_eq!(resp.access_token, "acc");
        assert_eq!(resp.refresh_token, "ref");
        assert_eq!(resp.user.username, "alice");
        assert_eq!(resp.user.email.as_deref(), Some("alice@example.com"));
    }

    #[test]
    fn login_response_parses_when_optional_user_fields_omitted() {
        // The server skips `email`, `avatar_url`, and `oauth_provider` when
        // they are `None`; the client must still deserialize the user.
        let json = r#"{
            "access_token": "acc",
            "refresh_token": "ref",
            "user": {
                "id": "00000000-0000-0000-0000-000000000001",
                "username": "bob",
                "status": "offline",
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z"
            }
        }"#;
        let resp: LoginResponse = serde_json::from_str(json).expect("parse");
        assert_eq!(resp.user.username, "bob");
        assert!(resp.user.email.is_none());
        assert!(resp.user.avatar_url.is_none());
        assert!(resp.user.oauth_provider.is_none());
    }

    #[test]
    fn error_body_parses() {
        let body: ErrorBody =
            serde_json::from_str(r#"{"error":"invalid email or password"}"#).expect("parse");
        assert_eq!(body.error, "invalid email or password");
    }
}
