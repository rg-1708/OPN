//! notify HTTP reads (OPN-CORE.md §10.8). The inbox is read on login; live
//! recipients get pushes over WS instead.

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

use crate::http::auth::JwtIdentity;
use crate::http::tenant::fail_response;
use crate::primitives::notify;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct InboxQuery {
    /// Newest-N. The cursor idiom replaces this in Sprint 4.
    pub limit: Option<i64>,
}

/// `GET /v1/notify/inbox?limit` — the caller's newest notifications.
pub async fn inbox(
    State(state): State<AppState>,
    JwtIdentity(who): JwtIdentity,
    Query(q): Query<InboxQuery>,
) -> Response {
    match notify::inbox_list(&state.pg, &who, q.limit.unwrap_or(50)).await {
        Ok(items) => Json(items).into_response(),
        Err(f) => fail_response(f),
    }
}
