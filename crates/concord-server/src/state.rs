use oauth2::basic::BasicClient;
use oauth2::{EndpointNotSet, EndpointSet};
use secrecy::SecretString;
use sqlx::PgPool;
use tokio::sync::broadcast;
use uuid::Uuid;

use concord_shared::protocol::ServerMsg;

pub type ConfiguredOAuthClient =
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

pub struct AppState {
    pub pool: PgPool,
    pub tx: broadcast::Sender<(Uuid, ServerMsg)>,
    pub jwt_secret: SecretString,
    pub github_oauth: Option<ConfiguredOAuthClient>,
    pub google_oauth: Option<ConfiguredOAuthClient>,
    pub http_client: reqwest::Client,
}
