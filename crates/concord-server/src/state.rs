use secrecy::SecretString;
use sqlx::PgPool;
use tokio::sync::broadcast;
use uuid::Uuid;

use concord_shared::protocol::ServerMsg;

pub struct AppState {
    pub pool: PgPool,
    pub tx: broadcast::Sender<(Uuid, ServerMsg)>,
    pub jwt_secret: SecretString,
}
