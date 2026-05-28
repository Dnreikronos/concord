use std::sync::Arc;

use sqlx::postgres::PgPoolOptions;

use concord_server::config::Config;
use concord_server::hub::Hub;
use concord_server::routes;
use concord_server::state::AppState;
use concord_server::typing::{self, Typing, SWEEP_INTERVAL, TYPING_TTL};

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

    let google_oauth = cfg.google_oauth.map(|g| {
        use oauth2::{basic::BasicClient, AuthUrl, ClientId, ClientSecret, RedirectUrl, TokenUrl};
        use secrecy::ExposeSecret;
        BasicClient::new(ClientId::new(g.client_id))
            .set_client_secret(ClientSecret::new(g.client_secret.expose_secret().to_string()))
            .set_auth_uri(
                AuthUrl::new("https://accounts.google.com/o/oauth2/v2/auth".into()).unwrap(),
            )
            .set_token_uri(
                TokenUrl::new("https://oauth2.googleapis.com/token".into()).unwrap(),
            )
            .set_redirect_uri(RedirectUrl::new(g.redirect_url).unwrap())
    });

    // Optional Redis: a connection manager for publishing typing events and a
    // raw client for the pub/sub subscriber. On any failure we log and fall
    // back to in-process fan-out rather than refusing to start.
    let (redis_publisher, redis_client) = match cfg.redis_url.as_deref() {
        None => (None, None),
        Some(url) => match redis::Client::open(url) {
            Ok(client) => match redis::aio::ConnectionManager::new(client.clone()).await {
                Ok(manager) => (Some(manager), Some(client)),
                Err(e) => {
                    eprintln!("redis connection failed ({e}); typing falls back to in-process");
                    (None, None)
                }
            },
            Err(e) => {
                eprintln!("invalid REDIS_URL ({e}); typing falls back to in-process");
                (None, None)
            }
        },
    };

    let hub = Arc::new(Hub::new());
    let typing = Arc::new(Typing::new(Arc::clone(&hub), TYPING_TTL, redis_publisher));
    Arc::clone(&typing).spawn_sweeper(SWEEP_INTERVAL);
    if let Some(client) = redis_client {
        typing::spawn_subscriber(client, Arc::clone(&typing));
    }

    let state = Arc::new(AppState {
        pool,
        hub,
        typing,
        jwt_secret: cfg.jwt_secret.into(),
        github_oauth,
        google_oauth,
        http_client: reqwest::Client::new(),
        ws_auth_timeout: std::time::Duration::from_secs(10),
    });

    let app = routes::all_routes().with_state(state);

    let listener = tokio::net::TcpListener::bind(cfg.addr)
        .await
        .expect("failed to bind");

    eprintln!("listening on {}", cfg.addr);
    axum::serve(listener, app).await.expect("server error");
}
