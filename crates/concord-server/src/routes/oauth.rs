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

const CSRF_COOKIE_NAME: &str = "__Secure-oauth_state";

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/github", get(github_redirect))
        .route("/github/callback", get(github_callback))
        .route("/google", get(google_redirect))
        .route("/google/callback", get(google_callback))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CallbackQuery {
    code: String,
    state: String,
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

fn set_csrf_cookie(csrf_state: &str, callback_path: &str) -> Result<HeaderMap, AppError> {
    let cookie = format!(
        "{CSRF_COOKIE_NAME}={csrf_state}; HttpOnly; Secure; SameSite=Lax; Path={callback_path}; Max-Age=300"
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        SET_COOKIE,
        cookie.parse().map_err(|_| AppError::Internal("invalid cookie header".into()))?,
    );
    Ok(headers)
}

fn clear_csrf_cookie(callback_path: &str) -> Result<HeaderMap, AppError> {
    let cookie = format!(
        "{CSRF_COOKIE_NAME}=; HttpOnly; Secure; SameSite=Lax; Path={callback_path}; Max-Age=0"
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        SET_COOKIE,
        cookie.parse().map_err(|_| AppError::Internal("invalid cookie header".into()))?,
    );
    Ok(headers)
}

fn verify_csrf(headers: &HeaderMap, query_state: &str, jwt_secret: &str) -> Result<(), AppError> {
    let cookie_state = extract_csrf_cookie(headers).ok_or(AppError::InvalidToken)?;

    let cookie_claims =
        jwt::decode_oauth_state(&cookie_state, jwt_secret).map_err(|_| AppError::InvalidToken)?;
    let query_claims =
        jwt::decode_oauth_state(query_state, jwt_secret).map_err(|_| AppError::InvalidToken)?;

    if cookie_claims.nonce != query_claims.nonce {
        return Err(AppError::InvalidToken);
    }
    Ok(())
}

fn issue_tokens(
    state: &AppState,
    user: &User,
) -> Result<(String, jwt::RefreshToken), AppError> {
    let access_token = jwt::encode_access_token(user.id, state.jwt_secret.expose_secret())
        .map_err(|e| AppError::Internal(e.to_string()))?;
    let refresh = jwt::generate_refresh_token();
    Ok((access_token, refresh))
}

fn sanitize_username(raw: &str, prefix: &str) -> String {
    let base: String = raw
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .take(28)
        .collect();

    if base.len() < 3 {
        format!("{prefix}_{base}")
    } else {
        base
    }
}

async fn find_or_create_oauth_user(
    pool: &PgPool,
    provider: &str,
    subject: &str,
    display_name: &str,
    email: Option<&str>,
    avatar_url: Option<&str>,
    username_prefix: &str,
) -> Result<User, AppError> {
    if let Some(user) = db::get_user_by_oauth(pool, provider, subject).await? {
        return Ok(user);
    }

    let base = sanitize_username(display_name, username_prefix);
    let candidates =
        std::iter::once(base.clone()).chain((1..=999).map(|i| format!("{base}_{i}")));

    for candidate in candidates {
        if candidate.len() > 32 {
            continue;
        }
        match db::insert_oauth_user(pool, &candidate, email, avatar_url, provider, subject).await {
            Ok(user) => return Ok(user),
            Err(AppError::UsernameExists) => continue,
            Err(AppError::Internal(ref msg)) if msg.contains("users_oauth_identity_idx") => {
                return db::get_user_by_oauth(pool, provider, subject)
                    .await?
                    .ok_or_else(|| AppError::Internal("oauth user vanished".into()));
            }
            Err(e) => return Err(e),
        }
    }

    Err(AppError::Internal("could not generate unique username".into()))
}

// ---------------------------------------------------------------------------
// GitHub
// ---------------------------------------------------------------------------

async fn github_redirect(
    State(state): State<Arc<AppState>>,
) -> Result<Response, AppError> {
    let client = state.github_oauth.as_ref().ok_or(AppError::OAuthNotConfigured)?;

    let csrf_state =
        jwt::encode_oauth_state(state.jwt_secret.expose_secret())
            .map_err(|e| AppError::Internal(e.to_string()))?;

    let headers = set_csrf_cookie(&csrf_state, "/api/auth/oauth/github/callback")?;

    let (auth_url, _) = client
        .authorize_url(|| oauth2::CsrfToken::new(csrf_state))
        .add_scope(oauth2::Scope::new("read:user".into()))
        .add_scope(oauth2::Scope::new("user:email".into()))
        .url();

    Ok((headers, Redirect::temporary(auth_url.as_str())).into_response())
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

async fn github_callback(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Result<(HeaderMap, Json<LoginResponse>), AppError> {
    let client = state.github_oauth.as_ref().ok_or(AppError::OAuthNotConfigured)?;

    verify_csrf(&headers, &query.state, state.jwt_secret.expose_secret())?;

    let token_response = client
        .exchange_code(oauth2::AuthorizationCode::new(query.code))
        .request_async(&state.http_client)
        .await
        .map_err(|e| AppError::OAuthFailed(format!("token exchange: {e}")))?;

    let gh_access_token = token_response.access_token().secret();

    let github_user: GitHubUser = state.http_client
        .get("https://api.github.com/user")
        .bearer_auth(gh_access_token)
        .header("User-Agent", "concord-server")
        .send()
        .await
        .map_err(|e| AppError::OAuthFailed(format!("user fetch: {e}")))?
        .error_for_status()
        .map_err(|e| AppError::OAuthFailed(format!("user fetch: {e}")))?
        .json()
        .await
        .map_err(|e| AppError::OAuthFailed(format!("user parse: {e}")))?;

    let email = match github_user.email {
        Some(ref e) if !e.is_empty() => Some(e.clone()),
        _ => fetch_github_primary_email(&state.http_client, gh_access_token).await?,
    };

    let user = find_or_create_oauth_user(
        &state.pool,
        "github",
        &github_user.id.to_string(),
        &github_user.login,
        email.as_deref(),
        github_user.avatar_url.as_deref(),
        "gh",
    )
    .await?;

    let (access_token, refresh) = issue_tokens(&state, &user)?;
    db::insert_refresh_token(&state.pool, user.id, &refresh.hash, refresh.expires_at).await?;

    let resp_headers = clear_csrf_cookie("/api/auth/oauth/github/callback")?;

    Ok((
        resp_headers,
        Json(LoginResponse {
            access_token,
            refresh_token: refresh.raw,
            user,
        }),
    ))
}

async fn fetch_github_primary_email(
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
        .error_for_status()
        .map_err(|e| AppError::OAuthFailed(format!("email fetch: {e}")))?
        .json()
        .await
        .map_err(|e| AppError::OAuthFailed(format!("email parse: {e}")))?;

    Ok(emails.into_iter().find(|e| e.primary && e.verified).map(|e| e.email))
}

// ---------------------------------------------------------------------------
// Google
// ---------------------------------------------------------------------------

async fn google_redirect(
    State(state): State<Arc<AppState>>,
) -> Result<Response, AppError> {
    let client = state.google_oauth.as_ref().ok_or(AppError::OAuthNotConfigured)?;

    let csrf_state =
        jwt::encode_oauth_state(state.jwt_secret.expose_secret())
            .map_err(|e| AppError::Internal(e.to_string()))?;

    let headers = set_csrf_cookie(&csrf_state, "/api/auth/oauth/google/callback")?;

    let (auth_url, _) = client
        .authorize_url(|| oauth2::CsrfToken::new(csrf_state))
        .add_scope(oauth2::Scope::new("openid".into()))
        .add_scope(oauth2::Scope::new("email".into()))
        .add_scope(oauth2::Scope::new("profile".into()))
        .url();

    Ok((headers, Redirect::temporary(auth_url.as_str())).into_response())
}

#[derive(Deserialize)]
struct GoogleUserInfo {
    sub: String,
    name: Option<String>,
    email: Option<String>,
    email_verified: Option<bool>,
    picture: Option<String>,
}

async fn google_callback(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Result<(HeaderMap, Json<LoginResponse>), AppError> {
    let client = state.google_oauth.as_ref().ok_or(AppError::OAuthNotConfigured)?;

    verify_csrf(&headers, &query.state, state.jwt_secret.expose_secret())?;

    let token_response = client
        .exchange_code(oauth2::AuthorizationCode::new(query.code))
        .request_async(&state.http_client)
        .await
        .map_err(|e| AppError::OAuthFailed(format!("token exchange: {e}")))?;

    let access_token_str = token_response.access_token().secret();

    let google_user: GoogleUserInfo = state.http_client
        .get("https://www.googleapis.com/oauth2/v3/userinfo")
        .bearer_auth(access_token_str)
        .send()
        .await
        .map_err(|e| AppError::OAuthFailed(format!("user fetch: {e}")))?
        .error_for_status()
        .map_err(|e| AppError::OAuthFailed(format!("user fetch: {e}")))?
        .json()
        .await
        .map_err(|e| AppError::OAuthFailed(format!("user parse: {e}")))?;

    let email = match (&google_user.email, google_user.email_verified) {
        (Some(e), Some(true)) => Some(e.clone()),
        _ => None,
    };

    let email_prefix = email.as_deref().and_then(|e| e.split('@').next());
    let display_name = google_user
        .name
        .as_deref()
        .or(email_prefix)
        .unwrap_or("user");

    let user = find_or_create_oauth_user(
        &state.pool,
        "google",
        &google_user.sub,
        display_name,
        email.as_deref(),
        google_user.picture.as_deref(),
        "g",
    )
    .await?;

    let (access_token, refresh) = issue_tokens(&state, &user)?;
    db::insert_refresh_token(&state.pool, user.id, &refresh.hash, refresh.expires_at).await?;

    let resp_headers = clear_csrf_cookie("/api/auth/oauth/google/callback")?;

    Ok((
        resp_headers,
        Json(LoginResponse {
            access_token,
            refresh_token: refresh.raw,
            user,
        }),
    ))
}
