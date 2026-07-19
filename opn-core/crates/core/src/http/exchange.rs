//! Framework exchange HTTP (OPN-CORE.md §10.5 item 4, OPN.md §14.2) — the
//! bridge-facing seam, API-key authed via `TenantAuth` (§11). The in-game app
//! initiates a withdraw over WS (`ledger.withdraw`); everything the *bridge*
//! drives — deposits, withdraw confirmation, and the reconciliation journal —
//! lives here. See `docs/opn-bridge-exchange.md` for the wire contract.

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::http::tenant::{err_response, fail_response, TenantAuth};
use crate::primitives::ledger;
use crate::state::AppState;
use contracts::ErrCode;

/// The two bridge-driven exchange actions. A `withdraw` is *started* over WS
/// (`ledger.withdraw`); the bridge only ever *confirms* it here.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ExchangeDirection {
    Deposit,
    WithdrawConfirm,
}

#[derive(Deserialize)]
pub struct ExchangeRequest {
    /// Idempotency key + audit id. Bridge-chosen for a deposit; for a
    /// withdraw_confirm it is the Core-minted id the `ledger.withdraw` ack
    /// returned.
    exchange_id: String,
    character_id: Uuid,
    amount: i64,
    direction: ExchangeDirection,
}

/// `POST /v1/tenants/self/exchange` — deposit or confirm a withdraw. Idempotent
/// on `exchange_id`. Ack `{ exchange_id, state, amount }`.
pub async fn exchange(
    State(state): State<AppState>,
    tenant: TenantAuth,
    Json(body): Json<ExchangeRequest>,
) -> Response {
    if body.exchange_id.is_empty() || body.exchange_id.len() > 128 {
        return err_response(ErrCode::Invalid, "exchange_id must be 1..=128 chars");
    }
    if body.amount <= 0 {
        return err_response(ErrCode::Invalid, "amount must be positive");
    }
    match body.direction {
        ExchangeDirection::Deposit => match ledger::deposit(
            &state,
            tenant.world_id,
            tenant.tenant_id,
            &body.exchange_id,
            body.character_id,
            body.amount,
        )
        .await
        {
            Ok(out) => Json(json!({
                "exchange_id": body.exchange_id,
                "state": out.state,
                "amount": out.amount,
            }))
            .into_response(),
            Err(f) => fail_response(f),
        },
        ExchangeDirection::WithdrawConfirm => match ledger::withdraw_confirm(
            &state,
            tenant.world_id,
            tenant.tenant_id,
            &body.exchange_id,
            body.character_id,
            body.amount,
        )
        .await
        {
            Ok(out) => Json(json!({
                "exchange_id": body.exchange_id,
                "state": out.state,
                "amount": out.amount,
            }))
            .into_response(),
            Err(f) => fail_response(f),
        },
    }
}

#[derive(Deserialize)]
pub struct JournalQuery {
    /// RFC 3339 lower bound (inclusive) on `created_at`; absent = from the start.
    since: Option<String>,
    /// Page size, capped at 500 by the store.
    limit: Option<i64>,
}

/// `GET /v1/tenants/self/exchange?since&limit` — the world's exchange journal for
/// the bridge's reconciliation, oldest-first. `{ items }`.
pub async fn journal(
    State(state): State<AppState>,
    tenant: TenantAuth,
    Query(q): Query<JournalQuery>,
) -> Response {
    let since = match q.since.as_deref() {
        Some(s) => match OffsetDateTime::parse(s, &Rfc3339) {
            Ok(t) => t,
            Err(_) => return err_response(ErrCode::Invalid, "since must be RFC 3339"),
        },
        None => OffsetDateTime::UNIX_EPOCH,
    };
    match ledger::exchange::journal(&state.pg, tenant.world_id, since, q.limit.unwrap_or(100)).await
    {
        Ok(items) => Json(json!({ "items": items })).into_response(),
        Err(f) => fail_response(f),
    }
}
