//! Authenticated reads against the server's REST API.
//!
//! The companion to [`crate::auth`]: where that module signs the user in, this
//! one fetches the data the main screen renders — the user's servers, a
//! server's channels, a channel's message history, and the user's DM
//! conversations. Like the auth HTTP calls, these live behind the `gui` feature
//! because `reqwest` is only pulled in for the desktop client.
//!
//! Every request carries the session's access token as a bearer credential and
//! decodes into the shared wire types from `concord_shared`.

use concord_shared::types::{Channel, DmChannelInfo, MessageWithAuthor, Server};
use serde::Deserialize;
use uuid::Uuid;

/// Why an API read failed.
#[derive(Debug, Clone)]
pub enum ApiError {
    /// The request never completed (DNS, connection refused, timeout, ...).
    Network(String),
    /// The server answered with a non-success status.
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

/// `GET base_url + path` with the bearer `token`, decoding the JSON body as `T`.
async fn get_json<T: serde::de::DeserializeOwned>(
    base_url: &str,
    token: &str,
    path: &str,
) -> Result<T, ApiError> {
    let base = base_url.trim_end_matches('/');
    let resp = crate::auth::http_client()
        .get(format!("{base}{path}"))
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
/// status code if the body isn't the expected envelope.
async fn server_error(resp: reqwest::Response) -> ApiError {
    let status = resp.status();
    match resp.json::<ErrorBody>().await {
        Ok(body) => ApiError::Server(body.error),
        Err(_) => ApiError::Server(format!("request failed ({status})")),
    }
}

/// `GET /api/servers` — the servers the signed-in user belongs to.
pub async fn list_servers(base_url: &str, token: &str) -> Result<Vec<Server>, ApiError> {
    get_json(base_url, token, "/api/servers").await
}

/// `GET /api/servers/{id}/channels` — the channels of a server the user is in.
pub async fn list_channels(
    base_url: &str,
    token: &str,
    server_id: Uuid,
) -> Result<Vec<Channel>, ApiError> {
    get_json(base_url, token, &format!("/api/servers/{server_id}/channels")).await
}

/// `GET /api/channels/{id}/messages` — newest-first history for a channel.
/// Works for both server channels and DM channels; access is gated server-side
/// on membership of whichever kind `id` names.
pub async fn list_messages(
    base_url: &str,
    token: &str,
    channel_id: Uuid,
) -> Result<Vec<MessageWithAuthor>, ApiError> {
    get_json(base_url, token, &format!("/api/channels/{channel_id}/messages")).await
}

/// `GET /api/dms` — the user's DM conversations, newest first.
pub async fn list_dms(base_url: &str, token: &str) -> Result<Vec<DmChannelInfo>, ApiError> {
    get_json(base_url, token, "/api/dms").await
}
