//! ledger primitive (OPN-CORE.md §10.5): accounts, transfers, and holds — money
//! that cannot be created, destroyed, or double-spent. `store.rs` owns the SQL
//! (deadlock-free locking, the available-balance math, the reconciliation
//! invariant); `fsm.rs` owns the hold state machine. This module validates,
//! calls the store, and routes the incoming-money notification. Exchange
//! (deposit / withdraw + the `exchanges` table) is Sprint 7 part B.

pub mod exchange;
pub mod fsm;
pub mod store;

use contracts::{ErrCode, NotifyClass};
use serde_json::json;
use uuid::Uuid;

use super::notify::{self, Notification};
use super::Fail;
use crate::infra::auth::Identity;
use crate::state::AppState;

/// Max hold lifetime: bounds `expires_in_secs` so a pathological value can't
/// overflow `make_interval` into an `internal` (the directory `ttl_secs` lesson,
/// reflections Sprint 5B).
const HOLD_MAX_SECS: i64 = 90 * 24 * 60 * 60; // 90 days

/// `ledger.transfer` (§10.5): debit the caller's `from` account, credit `to`,
/// notify the recipient (incoming money → `alert`). Idempotent on `client_uuid`.
/// Ack `{ transfer_id, balance }` (the source's new balance).
pub async fn transfer(
    state: &AppState,
    who: &Identity,
    from: Uuid,
    to: Uuid,
    amount: i64,
    client_uuid: Uuid,
) -> Result<serde_json::Value, Fail> {
    if amount <= 0 {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    // Reject the nil UUID: it is a real value (not SQL NULL), so it participates
    // in the `transfers_idem` index. A client that left `client_uuid`
    // zero-initialized would have its FIRST nil-keyed transfer stick and every
    // later nil-keyed one silently replay it — money not moving while the caller
    // is told it did. Fail loud instead (adversarial review, Sprint 7A keeper).
    if client_uuid.is_nil() {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    let out = store::transfer(
        &state.pg,
        who.world_id,
        who.character_id,
        from,
        to,
        amount,
        client_uuid,
    )
    .await?;
    if out.fresh {
        notify_incoming(state, who.world_id, out.to_owner, out.amount).await;
    }
    Ok(json!({ "transfer_id": out.transfer_id, "balance": out.from_balance }))
}

/// `ledger.hold` (§10.5): reserve funds on the caller's own account. Ack
/// `{ hold_id }`.
pub async fn hold(
    state: &AppState,
    who: &Identity,
    account: Uuid,
    amount: i64,
    expires_in_secs: i64,
) -> Result<serde_json::Value, Fail> {
    if amount <= 0 || expires_in_secs <= 0 || expires_in_secs > HOLD_MAX_SECS {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    let hold_id = store::hold(
        &state.pg,
        who.world_id,
        who.character_id,
        account,
        amount,
        expires_in_secs,
    )
    .await?;
    Ok(json!({ "hold_id": hold_id }))
}

/// `ledger.capture { hold_id, to }` (§10.5): settle a hold to a destination and
/// notify the recipient. Ack `{ transfer_id }`.
pub async fn capture(
    state: &AppState,
    who: &Identity,
    hold_id: Uuid,
    to: Uuid,
) -> Result<serde_json::Value, Fail> {
    let out = store::capture(&state.pg, who.world_id, who.character_id, hold_id, to).await?;
    notify_incoming(state, who.world_id, out.to_owner, out.amount).await;
    Ok(json!({ "transfer_id": out.transfer_id }))
}

/// `ledger.release { hold_id }` (§10.5): free a hold.
pub async fn release(state: &AppState, who: &Identity, hold_id: Uuid) -> Result<(), Fail> {
    store::release(&state.pg, who.world_id, who.character_id, hold_id).await
}

/// `ledger.withdraw { amount }` (§10.5 item 4), WS leg 1 of the framework
/// withdraw: reserve the caller's wallet with a hold and open a `pending_confirm`
/// exchange. Ack `{ exchange_id }` — the client relays it to the bridge, which
/// confirms via HTTP. Currency comes from the caller's tenant config.
pub async fn withdraw(
    state: &AppState,
    who: &Identity,
    amount: i64,
) -> Result<serde_json::Value, Fail> {
    let currency = exchange::tenant_currency(&state.pg, who.tenant_id).await?;
    let exchange_id =
        exchange::withdraw(&state.pg, who.world_id, who.character_id, &currency, amount).await?;
    Ok(json!({ "exchange_id": exchange_id }))
}

/// Exchange deposit (§10.5 item 4), the HTTP bridge path: credit `character`'s
/// wallet from the world's `system` account, idempotent on `exchange_id`. On a
/// fresh credit, notify the character of incoming money (item 8). Currency comes
/// from the tenant config.
pub async fn deposit(
    state: &AppState,
    world: Uuid,
    tenant: Uuid,
    exchange_id: &str,
    character: Uuid,
    amount: i64,
) -> Result<exchange::DepositOutcome, Fail> {
    let currency = exchange::tenant_currency(&state.pg, tenant).await?;
    let out =
        exchange::deposit(&state.pg, world, exchange_id, character, amount, &currency).await?;
    if out.fresh {
        notify_incoming(state, world, out.credited, out.amount).await;
    }
    Ok(out)
}

/// Exchange withdraw_confirm (§10.5 item 4), the HTTP bridge path: settle the
/// pending withdraw's hold to the `system` account. Idempotent on the exchange's
/// terminal `done` state. Currency comes from the tenant config.
pub async fn withdraw_confirm(
    state: &AppState,
    world: Uuid,
    tenant: Uuid,
    exchange_id: &str,
    character: Uuid,
    amount: i64,
) -> Result<exchange::ConfirmOutcome, Fail> {
    let currency = exchange::tenant_currency(&state.pg, tenant).await?;
    exchange::withdraw_confirm(&state.pg, world, exchange_id, character, amount, &currency).await
}

/// Notify a credited character of incoming money (§10.5 item 8): class `alert`,
/// app_id `wallet`. Best-effort — a failed notify never fails the transfer. A
/// system destination (`to_owner == None`) has no character to notify.
async fn notify_incoming(state: &AppState, world: Uuid, to_owner: Option<Uuid>, amount: i64) {
    let Some(recipient) = to_owner else {
        return;
    };
    let n = Notification {
        app_id: "wallet".into(),
        kind: "transfer_in".into(),
        class: NotifyClass::Alert,
        payload: json!({ "amount": amount }),
    };
    if let Err(e) = notify::route(state, world, recipient, n, false).await {
        tracing::error!(error = ?e, %recipient, "ledger incoming-transfer notify failed");
    }
}
