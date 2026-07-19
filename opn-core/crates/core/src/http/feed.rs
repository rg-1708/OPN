//! feed HTTP reads (OPN-CORE.md §10.3, Sprint 8 **part B**): the fan-out-on-read
//! surface — home timeline, profile timeline, post detail + comments, hashtag
//! page. All JWT-authed, all on the shared time cursor (CDR-7), newest first.
//! Feed is app-scoped, so every read takes `?app_id`; the store gates it (home
//! acts as the caller's active account, the rest require app membership).

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::http::auth::JwtIdentity;
use crate::http::tenant::fail_response;
use crate::infra::cursor::{self, Cursor};
use crate::primitives::feed::read;
use crate::primitives::Fail;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct FeedQuery {
    /// The app whose feed to read. Required; the store rejects an empty/oversize
    /// slug as `invalid` and a non-member as `forbidden`.
    pub app_id: Option<String>,
    /// Opaque keyset cursor from the previous page's `next_cursor` (CDR-7).
    pub cursor: Option<String>,
    /// Page size, capped at 100 by the store.
    pub limit: Option<i64>,
}

impl FeedQuery {
    /// `(app_id, decoded cursor, limit)` — an absent `app_id` becomes `""`, which
    /// the store rejects as `invalid`; a malformed cursor fails here.
    fn parts(&self) -> Result<(&str, Option<Cursor>, i64), Fail> {
        let cursor = self.cursor.as_deref().map(cursor::decode).transpose()?;
        Ok((
            self.app_id.as_deref().unwrap_or(""),
            cursor,
            self.limit.unwrap_or(50),
        ))
    }
}

/// `GET /v1/feed/home?app_id&cursor&limit` — `{ items, next_cursor }`, the
/// caller's fan-out-on-read home timeline.
pub async fn home(
    State(state): State<AppState>,
    JwtIdentity(who): JwtIdentity,
    Query(q): Query<FeedQuery>,
) -> Response {
    let (app_id, cursor, limit) = match q.parts() {
        Ok(p) => p,
        Err(f) => return fail_response(f),
    };
    match read::home(&state, &who, app_id, cursor, limit).await {
        Ok(page) => {
            Json(json!({ "items": page.items, "next_cursor": page.next_cursor })).into_response()
        }
        Err(f) => fail_response(f),
    }
}

/// `GET /v1/feed/profile/:account?app_id&cursor&limit` — one author's posts.
pub async fn profile(
    State(state): State<AppState>,
    JwtIdentity(who): JwtIdentity,
    Path(author): Path<Uuid>,
    Query(q): Query<FeedQuery>,
) -> Response {
    let (app_id, cursor, limit) = match q.parts() {
        Ok(p) => p,
        Err(f) => return fail_response(f),
    };
    match read::profile(&state, &who, app_id, author, cursor, limit).await {
        Ok(page) => {
            Json(json!({ "items": page.items, "next_cursor": page.next_cursor })).into_response()
        }
        Err(f) => fail_response(f),
    }
}

/// `GET /v1/feed/posts/:id?app_id&cursor&limit` — the post plus a comment page.
pub async fn post_detail(
    State(state): State<AppState>,
    JwtIdentity(who): JwtIdentity,
    Path(post_id): Path<Uuid>,
    Query(q): Query<FeedQuery>,
) -> Response {
    let (app_id, cursor, limit) = match q.parts() {
        Ok(p) => p,
        Err(f) => return fail_response(f),
    };
    match read::post_detail(&state, &who, app_id, post_id, cursor, limit).await {
        Ok((post, comments)) => Json(json!({
            "post": post,
            "comments": comments.items,
            "next_cursor": comments.next_cursor,
        }))
        .into_response(),
        Err(f) => fail_response(f),
    }
}

/// `GET /v1/feed/hashtags/:tag?app_id&cursor&limit` — posts under one hashtag.
pub async fn hashtag(
    State(state): State<AppState>,
    JwtIdentity(who): JwtIdentity,
    Path(tag): Path<String>,
    Query(q): Query<FeedQuery>,
) -> Response {
    let (app_id, cursor, limit) = match q.parts() {
        Ok(p) => p,
        Err(f) => return fail_response(f),
    };
    match read::hashtag(&state, &who, app_id, &tag, cursor, limit).await {
        Ok(page) => {
            Json(json!({ "items": page.items, "next_cursor": page.next_cursor })).into_response()
        }
        Err(f) => fail_response(f),
    }
}
