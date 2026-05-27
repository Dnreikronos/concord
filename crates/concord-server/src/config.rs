use std::env;
use std::net::SocketAddr;

use secrecy::SecretString;

pub struct GitHubOAuthConfig {
    pub client_id: String,
    pub client_secret: SecretString,
    pub redirect_url: String,
}

impl GitHubOAuthConfig {
    fn from_env() -> Option<Self> {
        let client_id = env::var("GITHUB_OAUTH_CLIENT_ID").unwrap_or_default();
        if client_id.is_empty() {
            return None;
        }

        let client_secret = env::var("GITHUB_OAUTH_CLIENT_SECRET")
            .expect("GITHUB_OAUTH_CLIENT_SECRET must be set when GITHUB_OAUTH_CLIENT_ID is set");
        let redirect_url = env::var("GITHUB_OAUTH_REDIRECT_URL")
            .expect("GITHUB_OAUTH_REDIRECT_URL must be set when GITHUB_OAUTH_CLIENT_ID is set");

        Some(Self { client_id, client_secret: client_secret.into(), redirect_url })
    }
}

pub struct GoogleOAuthConfig {
    pub client_id: String,
    pub client_secret: SecretString,
    pub redirect_url: String,
}

impl GoogleOAuthConfig {
    fn from_env() -> Option<Self> {
        let client_id = env::var("GOOGLE_OAUTH_CLIENT_ID").unwrap_or_default();
        if client_id.is_empty() {
            return None;
        }

        let client_secret = env::var("GOOGLE_OAUTH_CLIENT_SECRET")
            .expect("GOOGLE_OAUTH_CLIENT_SECRET must be set when GOOGLE_OAUTH_CLIENT_ID is set");
        let redirect_url = env::var("GOOGLE_OAUTH_REDIRECT_URL")
            .expect("GOOGLE_OAUTH_REDIRECT_URL must be set when GOOGLE_OAUTH_CLIENT_ID is set");

        Some(Self { client_id, client_secret: client_secret.into(), redirect_url })
    }
}

pub struct Config {
    pub database_url: String,
    pub addr: SocketAddr,
    pub max_connections: u32,
    pub jwt_secret: String,
    pub github_oauth: Option<GitHubOAuthConfig>,
    pub google_oauth: Option<GoogleOAuthConfig>,
}

impl Config {
    pub fn from_env() -> Self {
        let database_url =
            env::var("DATABASE_URL").expect("DATABASE_URL must be set");
        let host = env::var("HOST").unwrap_or_else(|_| "0.0.0.0".into());
        let port: u16 = env::var("PORT")
            .unwrap_or_else(|_| "8080".into())
            .parse()
            .expect("PORT must be a valid u16");

        let addr: SocketAddr = format!("{host}:{port}")
            .parse()
            .expect("HOST:PORT must form a valid socket address");

        let max_connections: u32 = env::var("MAX_CONNECTIONS")
            .unwrap_or_else(|_| "10".into())
            .parse()
            .expect("MAX_CONNECTIONS must be a valid u32");

        let jwt_secret =
            env::var("JWT_SECRET").expect("JWT_SECRET must be set");

        assert!(
            jwt_secret.len() >= 32,
            "JWT_SECRET must be at least 32 bytes"
        );

        let github_oauth = GitHubOAuthConfig::from_env();
        let google_oauth = GoogleOAuthConfig::from_env();

        Self { database_url, addr, max_connections, jwt_secret, github_oauth, google_oauth }
    }
}
