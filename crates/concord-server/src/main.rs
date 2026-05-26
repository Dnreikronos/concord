use std::sync::Arc;

use sqlx::postgres::PgPoolOptions;
use tokio::sync::broadcast;

use concord_server::config::Config;
use concord_server::routes;
use concord_server::state::AppState;

#[tokio::main]
async fn main() {
    let cfg = Config::from_env();

    let pool = PgPoolOptions::new()
        .max_connections(cfg.max_connections)
        .connect(&cfg.database_url)
        .await
        .expect("failed to connect to database");

    let (tx, _) = broadcast::channel(256);
    let state = Arc::new(AppState { pool, tx });

    let app = routes::all_routes().with_state(state);

    let listener = tokio::net::TcpListener::bind(cfg.addr)
        .await
        .expect("failed to bind");

    eprintln!("listening on {}", cfg.addr);
    axum::serve(listener, app).await.expect("server error");
}
