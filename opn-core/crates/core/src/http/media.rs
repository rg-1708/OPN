//! media HTTP reads (OPN-CORE.md §10.6). The gallery is a cursor page of the
//! caller's own live media; each row carries short-lived presigned GET URLs so
//! the client fetches bytes straight from S3, never through Core.

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::http::auth::JwtIdentity;
use crate::http::tenant::fail_response;
use crate::infra::cursor;
use crate::primitives::media;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct ListQuery {
    /// Opaque keyset cursor from the previous page's `next_cursor` (CDR-7).
    pub cursor: Option<String>,
    /// Page size, capped at 100 by the handler.
    pub limit: Option<i64>,
}

/// `GET /v1/media?cursor&limit` — `{ items, next_cursor }`, newest first.
pub async fn list(
    State(state): State<AppState>,
    JwtIdentity(who): JwtIdentity,
    Query(q): Query<ListQuery>,
) -> Response {
    let cursor = match q.cursor.as_deref().map(cursor::decode).transpose() {
        Ok(c) => c,
        Err(f) => return fail_response(f),
    };
    match media::list(&state, &who, cursor, q.limit.unwrap_or(50)).await {
        Ok(page) => Json(json!({
            "items": page.items,
            "next_cursor": page.next_cursor,
        }))
        .into_response(),
        Err(f) => fail_response(f),
    }
}
