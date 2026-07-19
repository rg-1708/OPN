//! ledger HTTP reads (OPN-CORE.md §10.5): the caller's own transfer history, a
//! cursor page on the shared idiom (CDR-7). Writes are WS commands; only the
//! journal read is HTTP (JWT-authed, like the media gallery and inbox).

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::http::auth::JwtIdentity;
use crate::http::tenant::fail_response;
use crate::infra::cursor;
use crate::primitives::ledger;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct HistoryQuery {
    /// Opaque keyset cursor from the previous page's `next_cursor` (CDR-7).
    pub cursor: Option<String>,
    /// Page size, capped at 100 by the store.
    pub limit: Option<i64>,
}

/// `GET /v1/ledger/history?cursor&limit` — `{ items, next_cursor }`, newest first.
pub async fn history(
    State(state): State<AppState>,
    JwtIdentity(who): JwtIdentity,
    Query(q): Query<HistoryQuery>,
) -> Response {
    let cursor = match q.cursor.as_deref().map(cursor::decode).transpose() {
        Ok(c) => c,
        Err(f) => return fail_response(f),
    };
    match ledger::store::history(&state.pg, &who, cursor, q.limit.unwrap_or(50)).await {
        Ok(page) => Json(json!({
            "items": page.items,
            "next_cursor": page.next_cursor,
        }))
        .into_response(),
        Err(f) => fail_response(f),
    }
}
