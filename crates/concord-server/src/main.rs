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

    let github_oauth = cfg.github_oauth.map(|gh| {
        use oauth2::{basic::BasicClient, AuthUrl, ClientId, ClientSecret, RedirectUrl, TokenUrl};
        use secrecy::ExposeSecret;
        BasicClient::new(ClientId::new(gh.client_id))
            .set_client_secret(ClientSecret::new(gh.client_secret.expose_secret().to_string()))
            .set_auth_uri(
                AuthUrl::new("https://github.com/login/oauth/authorize".into()).unwrap(),
            )
            .set_token_uri(
                TokenUrl::new("https://github.com/login/oauth/access_token".into()).unwrap(),
            )
            .set_redirect_uri(RedirectUrl::new(gh.redirect_url).unwrap())
    });

    let (tx, _) = broadcast::channel(256);
    let state = Arc::new(AppState {
        pool,
        tx,
        jwt_secret: cfg.jwt_secret.into(),
        github_oauth,
        http_client: reqwest::Client::new(),
    });

    let app = routes::all_routes().with_state(state);

    let listener = tokio::net::TcpListener::bind(cfg.addr)
        .await
        .expect("failed to bind");

    eprintln!("listening on {}", cfg.addr);
    axum::serve(listener, app).await.expect("server error");
}
