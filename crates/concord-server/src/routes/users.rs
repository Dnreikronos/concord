use std::sync::Arc;

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;

use concord_shared::types::UserSummary;

use crate::db;
use crate::error::AppError;
use crate::extractors::AuthUser;
use crate::state::AppState;

/// Default number of matches returned when the caller gives no `limit`.
const DEFAULT_SEARCH_LIMIT: i64 = 20;
/// Upper bound on `limit`, so a caller can't ask for an unbounded scan.
const MAX_SEARCH_LIMIT: i64 = 50;

#[derive(Deserialize)]
struct SearchQuery {
    /// The username fragment to match, case-insensitively, as a substring.
    #[serde(default)]
    q: String,
    /// Page size; clamped to `[1, MAX_SEARCH_LIMIT]`, defaults to
    /// `DEFAULT_SEARCH_LIMIT`.
    #[serde(default)]
    limit: Option<i64>,
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/search", get(search_users))
}

/// `GET /api/users/search?q=&limit=` — find users by username, for the DM and
/// group-DM participant pickers.
///
/// Matches `q` as a case-insensitive substring of the username and never
/// returns the caller themselves. A blank `q` yields an empty list rather than
/// the whole user table, so an empty search box stays empty.
async fn search_users(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Vec<UserSummary>>, AppError> {
    let term = query.q.trim();
    if term.is_empty() {
        return Ok(Json(Vec::new()));
    }

    let limit = query
        .limit
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
        .clamp(1, MAX_SEARCH_LIMIT);

    let users = db::search_users_by_username(&state.pool, term, auth.user_id, limit).await?;
    Ok(Json(users))
}
