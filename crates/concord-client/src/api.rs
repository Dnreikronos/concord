//! REST client for loading the data the desktop UI renders.
//!
//! Mirrors [`crate::auth`] in shape: it reuses that module's shared, warmed
//! [`reqwest::Client`] and follows the same error-envelope handling. Every call
//! carries the access token as a `Bearer` header — these endpoints all sit
//! behind the server's auth middleware.
//!
//! This module also hosts the shared tokio runtime and the WebSocket-URL helper
//! the root view uses, since both belong to the same `gui`-only networking
//! layer.

use std::sync::OnceLock;

use serde::de::DeserializeOwned;
use serde::Deserialize;
use uuid::Uuid;

use concord_shared::types::{Channel, ChannelCategory, MemberInfo, MessageWithAuthor, Server};

use crate::auth::{api_base_url, http_client};

/// Shared tokio runtime that drives the `gui`-only network I/O (the REST loads
/// here and the WebSocket task), since GPUI runs its own non-tokio executor.
/// Results are handed back to GPUI tasks over channels.
pub(crate) fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Runtime::new().expect("failed to start tokio runtime for network requests")
    })
}

/// The WebSocket URL, from `CONCORD_WS_URL` or derived from the REST base by
/// swapping the scheme to `ws(s)` and appending `/ws` (the server serves REST
/// and the socket on the same port).
pub fn ws_url() -> String {
    if let Ok(url) = std::env::var("CONCORD_WS_URL") {
        return url;
    }
    let base = api_base_url();
    let base = base.trim_end_matches('/');
    let ws = base
        .strip_prefix("https://")
        .map(|rest| format!("wss://{rest}"))
        .or_else(|| base.strip_prefix("http://").map(|rest| format!("ws://{rest}")))
        .unwrap_or_else(|| base.to_string());
    format!("{ws}/ws")
}

/// Why a REST call failed.
#[derive(Debug, Clone)]
pub enum ApiError {
    /// The request never completed (DNS, connection refused, timeout, ...).
    Network(String),
    /// The server answered with a non-2xx status and an error message.
    Server(String),
    /// The server answered, but not in a shape we could understand.
    Unexpected(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network(m) => write!(f, "could not reach the server: {m}"),
            Self::Server(m) => write!(f, "{m}"),
            Self::Unexpected(m) => write!(f, "unexpected response: {m}"),
        }
    }
}

impl std::error::Error for ApiError {}

/// Server's error envelope: `{ "error": "..." }`.
#[derive(Deserialize)]
struct ErrorBody {
    error: String,
}

/// `GET base/path` with a Bearer token, decoding a successful body as `T`.
async fn get_json<T: DeserializeOwned>(
    base_url: &str,
    token: &str,
    path: &str,
) -> Result<T, ApiError> {
    let url = format!(
        "{}/{}",
        base_url.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    let resp = http_client()
        .get(url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| ApiError::Network(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(server_error(resp).await);
    }
    resp.json::<T>()
        .await
        .map_err(|e| ApiError::Unexpected(e.to_string()))
}

/// Read the server's error message off a non-2xx response, falling back to the
/// status code when the body isn't the expected envelope.
async fn server_error(resp: reqwest::Response) -> ApiError {
    let status = resp.status();
    match resp.json::<ErrorBody>().await {
        Ok(body) => ApiError::Server(body.error),
        Err(_) => ApiError::Server(format!("request failed ({status})")),
    }
}

/// `GET /api/servers` — the servers the authenticated user belongs to.
pub async fn list_servers(base_url: &str, token: &str) -> Result<Vec<Server>, ApiError> {
    get_json(base_url, token, "api/servers").await
}

/// `GET /api/servers/{id}/channels` — the channels of `server_id`.
pub async fn list_channels(
    base_url: &str,
    token: &str,
    server_id: Uuid,
) -> Result<Vec<Channel>, ApiError> {
    get_json(base_url, token, &format!("api/servers/{server_id}/channels")).await
}

/// `GET /api/servers/{id}/categories` — the channel categories of `server_id`.
pub async fn list_categories(
    base_url: &str,
    token: &str,
    server_id: Uuid,
) -> Result<Vec<ChannelCategory>, ApiError> {
    get_json(base_url, token, &format!("api/servers/{server_id}/categories")).await
}

/// `GET /api/servers/{id}/members` — the members of `server_id`.
pub async fn list_members(
    base_url: &str,
    token: &str,
    server_id: Uuid,
) -> Result<Vec<MemberInfo>, ApiError> {
    get_json(base_url, token, &format!("api/servers/{server_id}/members")).await
}

/// `GET /api/channels/{id}/messages` — cursor-paginated history, newest first.
///
/// `before` fetches messages older than that id; `limit` caps the page (the
/// server clamps it). Works for server channels and DM channels alike.
pub async fn list_messages(
    base_url: &str,
    token: &str,
    channel_id: Uuid,
    before: Option<Uuid>,
    limit: Option<i64>,
) -> Result<Vec<MessageWithAuthor>, ApiError> {
    let url = format!(
        "{}/api/channels/{channel_id}/messages",
        base_url.trim_end_matches('/')
    );
    let mut query: Vec<(&str, String)> = Vec::new();
    if let Some(before) = before {
        query.push(("before", before.to_string()));
    }
    if let Some(limit) = limit {
        query.push(("limit", limit.to_string()));
    }
    let resp = http_client()
        .get(url)
        .query(&query)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| ApiError::Network(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(server_error(resp).await);
    }
    resp.json()
        .await
        .map_err(|e| ApiError::Unexpected(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // A single test: `CONCORD_API_URL` / `CONCORD_WS_URL` are process-global, so
    // splitting these cases into parallel tests would race on the same vars.
    #[test]
    fn ws_url_resolution() {
        std::env::remove_var("CONCORD_WS_URL");

        std::env::set_var("CONCORD_API_URL", "http://127.0.0.1:8080");
        assert_eq!(ws_url(), "ws://127.0.0.1:8080/ws");

        std::env::set_var("CONCORD_API_URL", "https://chat.example.com/");
        assert_eq!(ws_url(), "wss://chat.example.com/ws");

        // An explicit override wins over the derived URL.
        std::env::set_var("CONCORD_WS_URL", "ws://override/socket");
        assert_eq!(ws_url(), "ws://override/socket");

        std::env::remove_var("CONCORD_WS_URL");
        std::env::remove_var("CONCORD_API_URL");
    }
}
