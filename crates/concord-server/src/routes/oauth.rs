use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::header::SET_COOKIE;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};
use oauth2::TokenResponse;
use secrecy::ExposeSecret;
use serde::Deserialize;
use sqlx::PgPool;

use concord_shared::types::User;

use crate::db;
use crate::error::AppError;
use crate::jwt;
use crate::routes::auth::LoginResponse;
use crate::state::AppState;

const CSRF_COOKIE_NAME: &str = "__Host-oauth_state";

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/github", get(github_redirect))
        .route("/github/callback", get(github_callback))
}

async fn github_redirect(
    State(state): State<Arc<AppState>>,
) -> Result<Response, AppError> {
    let client = state.github_oauth.as_ref().ok_or(AppError::OAuthNotConfigured)?;

    let csrf_state =
        jwt::encode_oauth_state(state.jwt_secret.expose_secret())
            .map_err(|e| AppError::Internal(e.to_string()))?;

    let cookie = format!(
        "{CSRF_COOKIE_NAME}={csrf_state}; HttpOnly; Secure; SameSite=Lax; Path=/api/auth/oauth/github/callback; Max-Age=300"
    );

    let (auth_url, _) = client
        .authorize_url(|| oauth2::CsrfToken::new(csrf_state))
        .add_scope(oauth2::Scope::new("read:user".into()))
        .add_scope(oauth2::Scope::new("user:email".into()))
        .url();

    let mut headers = HeaderMap::new();
    headers.insert(SET_COOKIE, cookie.parse().unwrap());

    Ok((headers, Redirect::temporary(auth_url.as_str())).into_response())
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: String,
    state: String,
}

#[derive(Deserialize)]
struct GitHubUser {
    id: u64,
    login: String,
    email: Option<String>,
    avatar_url: Option<String>,
}

#[derive(Deserialize)]
struct GitHubEmail {
    email: String,
    primary: bool,
    verified: bool,
}

fn extract_csrf_cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all("cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(';'))
        .map(|s| s.trim())
        .find_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            if name.trim() == CSRF_COOKIE_NAME {
                Some(value.trim().to_string())
            } else {
                None
            }
        })
}

async fn github_callback(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Result<(HeaderMap, Json<LoginResponse>), AppError> {
    let client = state.github_oauth.as_ref().ok_or(AppError::OAuthNotConfigured)?;

    let cookie_state = extract_csrf_cookie(&headers).ok_or(AppError::InvalidToken)?;
    if cookie_state != query.state {
        return Err(AppError::InvalidToken);
    }
    jwt::decode_oauth_state(&query.state, state.jwt_secret.expose_secret())
        .map_err(|_| AppError::InvalidToken)?;

    let http_client = reqwest::Client::new();
    let token_response = client
        .exchange_code(oauth2::AuthorizationCode::new(query.code))
        .request_async(&http_client)
        .await
        .map_err(|e| AppError::OAuthFailed(format!("token exchange: {e}")))?;

    let gh_access_token = token_response.access_token().secret();

    let github_user: GitHubUser = http_client
        .get("https://api.github.com/user")
        .bearer_auth(gh_access_token)
        .header("User-Agent", "concord-server")
        .send()
        .await
        .map_err(|e| AppError::OAuthFailed(format!("user fetch: {e}")))?
        .json()
        .await
        .map_err(|e| AppError::OAuthFailed(format!("user parse: {e}")))?;

    let email = match github_user.email {
        Some(ref e) if !e.is_empty() => Some(e.clone()),
        _ => fetch_primary_email(&http_client, gh_access_token).await?,
    };

    let github_subject = github_user.id.to_string();
    let user = find_or_create_github_user(
        &state.pool,
        &github_subject,
        &github_user.login,
        email.as_deref(),
        github_user.avatar_url.as_deref(),
    )
    .await?;

    let access_token =
        jwt::encode_access_token(user.id, state.jwt_secret.expose_secret())
            .map_err(|e| AppError::Internal(e.to_string()))?;

    let refresh = jwt::generate_refresh_token();
    db::insert_refresh_token(&state.pool, user.id, &refresh.hash, refresh.expires_at)
        .await?;

    let clear_cookie = format!(
        "{CSRF_COOKIE_NAME}=; HttpOnly; Secure; SameSite=Lax; Path=/api/auth/oauth/github/callback; Max-Age=0"
    );
    let mut resp_headers = HeaderMap::new();
    resp_headers.insert(SET_COOKIE, clear_cookie.parse().unwrap());

    Ok((
        resp_headers,
        Json(LoginResponse {
            access_token,
            refresh_token: refresh.raw,
            user,
        }),
    ))
}

async fn fetch_primary_email(
    http_client: &reqwest::Client,
    access_token: &str,
) -> Result<Option<String>, AppError> {
    let emails: Vec<GitHubEmail> = http_client
        .get("https://api.github.com/user/emails")
        .bearer_auth(access_token)
        .header("User-Agent", "concord-server")
        .send()
        .await
        .map_err(|e| AppError::OAuthFailed(format!("email fetch: {e}")))?
        .json()
        .await
        .map_err(|e| AppError::OAuthFailed(format!("email parse: {e}")))?;

    Ok(emails.into_iter().find(|e| e.primary && e.verified).map(|e| e.email))
}

async fn find_or_create_github_user(
    pool: &PgPool,
    github_subject: &str,
    github_login: &str,
    email: Option<&str>,
    avatar_url: Option<&str>,
) -> Result<User, AppError> {
    if let Some(user) = db::get_user_by_oauth(pool, "github", github_subject).await? {
        return Ok(user);
    }

    let base = sanitize_username(github_login);
    let candidates =
        std::iter::once(base.clone()).chain((1..=999).map(|i| format!("{base}_{i}")));

    for candidate in candidates {
        if candidate.len() > 32 {
            continue;
        }
        match db::insert_oauth_user(pool, &candidate, email, avatar_url, "github", github_subject)
            .await
        {
            Ok(user) => return Ok(user),
            Err(AppError::UsernameExists) => continue,
            // OAuth identity was inserted by a concurrent request between our
            // get_user_by_oauth check and this insert.
            Err(AppError::Internal(ref msg)) if msg.contains("users_oauth_identity_idx") => {
                return db::get_user_by_oauth(pool, "github", github_subject)
                    .await?
                    .ok_or_else(|| AppError::Internal("oauth user vanished".into()));
            }
            Err(e) => return Err(e),
        }
    }

    Err(AppError::Internal("could not generate unique username".into()))
}

fn sanitize_username(github_login: &str) -> String {
    let base: String = github_login
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .take(28)
        .collect();

    if base.len() < 3 {
        format!("gh_{base}")
    } else {
        base
    }
}
