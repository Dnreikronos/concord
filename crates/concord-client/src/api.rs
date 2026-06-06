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

use concord_shared::types::{
    Channel, ChannelCategory, DmChannelInfo, DmConversation, Friend, FriendRequests, MemberInfo,
    MessageWithAuthor, Server, UserSummary,
};

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

/// Send a prepared request and discard the body, mapping a non-2xx status to an
/// `ApiError`. For mutating calls (POST/DELETE) whose response we don't need.
async fn expect_success(req: reqwest::RequestBuilder) -> Result<(), ApiError> {
    let resp = req.send().await.map_err(|e| ApiError::Network(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(server_error(resp).await);
    }
    Ok(())
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

/// `GET /api/dms` — the caller's DM conversations, newest activity first. Each
/// row carries its participants, a last-message preview, and the unread flag.
pub async fn list_dms(base_url: &str, token: &str) -> Result<Vec<DmConversation>, ApiError> {
    get_json(base_url, token, "api/dms").await
}

/// `POST /api/dms/{id}/read` — mark a DM read for the caller as of now, clearing
/// its unread flag on the server until another member posts again.
pub async fn mark_dm_read(base_url: &str, token: &str, dm_channel_id: Uuid) -> Result<(), ApiError> {
    let url = format!(
        "{}/api/dms/{dm_channel_id}/read",
        base_url.trim_end_matches('/')
    );
    let resp = http_client()
        .post(url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| ApiError::Network(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(server_error(resp).await);
    }
    Ok(())
}

/// `GET /api/users/search?q=&limit=` — find users by username for the group-DM
/// participant picker. The server matches `query` as a case-insensitive
/// substring and never returns the caller themselves.
pub async fn search_users(
    base_url: &str,
    token: &str,
    query: &str,
    limit: i64,
) -> Result<Vec<UserSummary>, ApiError> {
    let url = format!("{}/api/users/search", base_url.trim_end_matches('/'));
    let limit = limit.to_string();
    let resp = http_client()
        .get(url)
        .query(&[("q", query), ("limit", limit.as_str())])
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

/// `POST /api/dms/group` — create a group DM owned by the caller, with the given
/// recipients and an optional name. The caller is always a participant, so
/// `recipient_ids` lists only the others; the server bounds the total at 2–10.
/// Returns the new channel with its participants resolved.
pub async fn create_group_dm(
    base_url: &str,
    token: &str,
    recipient_ids: &[Uuid],
    name: Option<&str>,
) -> Result<DmChannelInfo, ApiError> {
    let url = format!("{}/api/dms/group", base_url.trim_end_matches('/'));
    let body = serde_json::json!({ "recipient_ids": recipient_ids, "name": name });
    let resp = http_client()
        .post(url)
        .bearer_auth(token)
        .json(&body)
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

/// `POST /api/dms` — open (or reuse) a 1:1 DM with `recipient_id`. Returns the
/// channel with both participants resolved, whether freshly created or reused.
pub async fn create_dm(
    base_url: &str,
    token: &str,
    recipient_id: Uuid,
) -> Result<DmChannelInfo, ApiError> {
    let url = format!("{}/api/dms", base_url.trim_end_matches('/'));
    let body = serde_json::json!({ "recipient_id": recipient_id });
    let resp = http_client()
        .post(url)
        .bearer_auth(token)
        .json(&body)
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

/// `GET /api/friends` — the caller's accepted friends, each with live presence.
pub async fn list_friends(base_url: &str, token: &str) -> Result<Vec<Friend>, ApiError> {
    get_json(base_url, token, "api/friends").await
}

/// `GET /api/friends/requests` — the caller's pending requests, both directions.
pub async fn list_friend_requests(
    base_url: &str,
    token: &str,
) -> Result<FriendRequests, ApiError> {
    get_json(base_url, token, "api/friends/requests").await
}

/// Which way `POST /api/friends/requests` resolved: a new pending request, or an
/// immediate accept because the target had already requested the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FriendRequestOutcome {
    Requested,
    Accepted,
}

/// `POST /api/friends/requests` — send a friend request to `user_id`, or accept
/// their pending one to the caller (server-side). The outcome tells the caller
/// which happened, so it can refresh the right lists and show an honest message;
/// the rest of the body is reconciled via a refetch and live events.
pub async fn send_friend_request(
    base_url: &str,
    token: &str,
    user_id: Uuid,
) -> Result<FriendRequestOutcome, ApiError> {
    #[derive(Deserialize)]
    struct Resp {
        outcome: String,
    }
    let url = format!("{}/api/friends/requests", base_url.trim_end_matches('/'));
    let resp = http_client()
        .post(url)
        .bearer_auth(token)
        .json(&serde_json::json!({ "user_id": user_id }))
        .send()
        .await
        .map_err(|e| ApiError::Network(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(server_error(resp).await);
    }
    let body: Resp = resp.json().await.map_err(|e| ApiError::Unexpected(e.to_string()))?;
    Ok(match body.outcome.as_str() {
        "accepted" => FriendRequestOutcome::Accepted,
        _ => FriendRequestOutcome::Requested,
    })
}

/// `POST /api/friends/requests/{id}/accept` — accept an incoming request.
pub async fn accept_friend_request(
    base_url: &str,
    token: &str,
    request_id: Uuid,
) -> Result<(), ApiError> {
    let url = format!("{}/api/friends/requests/{request_id}/accept", base_url.trim_end_matches('/'));
    expect_success(http_client().post(url).bearer_auth(token)).await
}

/// `DELETE /api/friends/requests/{id}` — reject an incoming request or cancel an
/// outgoing one.
pub async fn delete_friend_request(
    base_url: &str,
    token: &str,
    request_id: Uuid,
) -> Result<(), ApiError> {
    let url = format!("{}/api/friends/requests/{request_id}", base_url.trim_end_matches('/'));
    expect_success(http_client().delete(url).bearer_auth(token)).await
}

/// `DELETE /api/friends/{user_id}` — unfriend `user_id`.
pub async fn remove_friend(base_url: &str, token: &str, user_id: Uuid) -> Result<(), ApiError> {
    let url = format!("{}/api/friends/{user_id}", base_url.trim_end_matches('/'));
    expect_success(http_client().delete(url).bearer_auth(token)).await
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
