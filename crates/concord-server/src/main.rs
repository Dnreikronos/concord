mod config;
mod db;
mod error;
mod routes;

use sqlx::postgres::PgPoolOptions;

use crate::config::Config;

#[tokio::main]
async fn main() {
    let cfg = Config::from_env();

    let pool = PgPoolOptions::new()
        .max_connections(cfg.max_connections)
        .connect(&cfg.database_url)
        .await
        .expect("failed to connect to database");

    let app = routes::all_routes().with_state(pool);

    let listener = tokio::net::TcpListener::bind(cfg.addr)
        .await
        .expect("failed to bind");

    eprintln!("listening on {}", cfg.addr);
    axum::serve(listener, app).await.expect("server error");
}
