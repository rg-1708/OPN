//! media HTTP reads (OPN-CORE.md §10.6). The gallery is a cursor page of the
//! caller's own live media; each row carries short-lived presigned GET URLs so
//! the client fetches bytes straight from S3, never through Core.

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::http::auth::JwtIdentity;
use crate::http::tenant::fail_response;
use crate::infra::cursor;
use crate::primitives::media;
use crate::primitives::Fail;
use crate::state::AppState;
use contracts::ErrCode;

#[derive(Deserialize)]
pub struct ListQuery {
    /// Opaque keyset cursor from the previous page's `next_cursor` (CDR-7).
    pub cursor: Option<String>,
    /// Page size, capped at 100 by the handler.
    pub limit: Option<i64>,
    /// Comma-separated media ids to resolve by id (gap #1). When present, the
    /// handler returns `{ items }` for exactly these live ids (world-scoped),
    /// ignoring `cursor`/`limit` — used to render attachments sent by others.
    pub ids: Option<String>,
}

/// `GET /v1/media` — two shapes:
/// * `?ids=a,b,c` → `{ items }`, the given live media ids resolved to presigned
///   URLs (gap #1), in request order, missing ids skipped.
/// * `?cursor&limit` → `{ items, next_cursor }`, the caller's own gallery,
///   newest first.
pub async fn list(
    State(state): State<AppState>,
    JwtIdentity(who): JwtIdentity,
    Query(q): Query<ListQuery>,
) -> Response {
    if let Some(raw) = q.ids.as_deref() {
        let ids: Result<Vec<Uuid>, _> = raw
            .split(',')
            .filter(|s| !s.is_empty())
            .map(Uuid::parse_str)
            .collect();
        let ids = match ids {
            Ok(ids) => ids,
            Err(_) => return fail_response(Fail::Code(ErrCode::Invalid)),
        };
        return match media::list_by_ids(&state, &who, &ids).await {
            Ok(items) => Json(json!({ "items": items })).into_response(),
            Err(f) => fail_response(f),
        };
    }
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
