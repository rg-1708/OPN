//! channels HTTP reads (OPN-CORE.md §6). History is the one seq-keyed read —
//! seq is already public in this contract, so it keysets on `before_seq`
//! rather than the opaque time cursor (which is for time-ordered surfaces).

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;

use crate::http::auth::JwtIdentity;
use crate::http::tenant::fail_response;
use crate::primitives::channels;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct HistoryQuery {
    /// Keyset: return messages with `seq <` this. Absent = newest page.
    pub before_seq: Option<i64>,
    /// Page size, capped at 100 by the handler.
    pub limit: Option<i64>,
}

/// `GET /v1/channels/:id/messages?before_seq&limit` — a membership-gated page
/// of messages, newest first.
pub async fn history(
    State(state): State<AppState>,
    JwtIdentity(who): JwtIdentity,
    Path(channel_id): Path<Uuid>,
    Query(q): Query<HistoryQuery>,
) -> Response {
    match channels::history(
        &state,
        &who,
        channel_id,
        q.before_seq,
        q.limit.unwrap_or(50),
    )
    .await
    {
        Ok(items) => Json(items).into_response(),
        Err(f) => fail_response(f),
    }
}
