use std::sync::Arc;
use std::time::Duration;

use oauth2::basic::BasicClient;
use oauth2::{EndpointNotSet, EndpointSet};
use secrecy::SecretString;
use sqlx::PgPool;

use crate::hub::Hub;
use crate::presence::Presence;
use crate::typing::Typing;

pub type ConfiguredOAuthClient =
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

pub struct AppState {
    pub pool: PgPool,
    pub hub: Arc<Hub>,
    /// Durable, TTL-backed presence store (Redis). Best-effort: a disabled or
    /// unreachable store degrades to in-process-only presence rather than
    /// breaking connections.
    pub presence: Presence,
    pub typing: Arc<Typing>,
    pub jwt_secret: SecretString,
    pub github_oauth: Option<ConfiguredOAuthClient>,
    pub google_oauth: Option<ConfiguredOAuthClient>,
    pub http_client: reqwest::Client,
    /// How long an unauthenticated WebSocket connection may stay open before the
    /// auth-first handshake must complete. Bounds the soft-DoS surface of idle
    /// or junk-trickling clients.
    pub ws_auth_timeout: Duration,
}
