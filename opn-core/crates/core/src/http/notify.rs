//! notify HTTP reads (OPN-CORE.md §10.8). The inbox is read on login; live
//! recipients get pushes over WS instead.

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::http::auth::JwtIdentity;
use crate::http::tenant::fail_response;
use crate::infra::cursor;
use crate::primitives::notify;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct InboxQuery {
    /// Opaque keyset cursor from the previous page's `next_cursor` (CDR-7).
    pub cursor: Option<String>,
    /// Page size, capped at 100 by the handler.
    pub limit: Option<i64>,
}

/// `GET /v1/notify/inbox?cursor&limit` — `{ items, next_cursor }`, newest first.
pub async fn inbox(
    State(state): State<AppState>,
    JwtIdentity(who): JwtIdentity,
    Query(q): Query<InboxQuery>,
) -> Response {
    let cursor = match q.cursor.as_deref().map(cursor::decode).transpose() {
        Ok(c) => c,
        Err(f) => return fail_response(f),
    };
    match notify::inbox_list(&state.pg, &who, cursor, q.limit.unwrap_or(50)).await {
        Ok(page) => Json(json!({
            "items": page.items,
            "next_cursor": page.next_cursor,
        }))
        .into_response(),
        Err(f) => fail_response(f),
    }
}
