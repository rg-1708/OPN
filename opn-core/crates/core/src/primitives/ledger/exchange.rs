//! Framework exchange (OPN-CORE.md §10.5 item 4, OPN.md §14.2) — Sprint 7 part
//! B. The one seam where value crosses between the framework bank and the ledger.
//! Builds directly on part A's transfer/hold machinery; the SQL here reuses the
//! same deadlock-free id-ordered locking and the same `held_sum` available-math.
//!
//! Two directions, both idempotent, both audited by an `exchanges` row:
//!   * **deposit** — the bridge credits a character wallet from the tenant
//!     `system` account (system → wallet, transfer `kind='deposit'`). Idempotent
//!     on the bridge-chosen exchange id.
//!   * **withdraw** — two-legged. `withdraw` (WS, leg 1) reserves the wallet with
//!     a hold and opens a `pending_confirm` exchange, returning a Core-minted id;
//!     `withdraw_confirm` (HTTP, leg 2, the bridge) captures the hold to `system`
//!     (wallet → system, transfer `kind='withdraw'`). An unconfirmed withdraw's
//!     hold expires (janitor) and the exchange auto-expires with it.
//!
//! The distinct transfer `kind`s are load-bearing: `store::reconcile`'s exchange
//! cross-check ([`cross_check`]) compares Σ(exchanges) against exactly the
//! 'deposit'/'withdraw' legs, so a plain transfer or a user hold-capture never
//! participates and an orphaned exchange (or a lost leg) freezes `system`.

use metrics::counter;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use super::fsm::{self, Action};
use super::store::{held_sum, hold_str, parse_hold};
use crate::infra::db::world_tx;
use crate::infra::ids::new_id;
use crate::infra::timefmt::rfc3339;
use crate::primitives::Fail;
use contracts::ErrCode;

/// How long a withdraw's hold lives before the janitor releases it and the
/// exchange auto-expires. The bridge must credit the framework bank and call
/// `withdraw_confirm` within this window.
/// ponytail: a const, not config — one hour fits every framework's confirm
/// round-trip. Promote to `OPN_WITHDRAW_CONFIRM_SECS` if an operator needs it.
const WITHDRAW_HOLD_SECS: i64 = 60 * 60;

/// Read a tenant's configured currency (§10.5 item 4). `tenants` is not
/// world-scoped (0003), so this is a plain-pool read of the granted column.
pub async fn tenant_currency(pool: &PgPool, tenant: Uuid) -> Result<String, Fail> {
    let cur: Option<String> = sqlx::query_scalar("SELECT currency FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_optional(pool)
        .await
        .map_err(|e| Fail::Internal(e.into()))?;
    cur.ok_or(Fail::Code(ErrCode::NotFound))
}

// ── get-or-create (idempotent under concurrent first-touch) ──────────────────

/// The world's `system` account for `currency`, created on first touch.
/// Race-safe on concurrent first-touch: `ON CONFLICT DO UPDATE` locks and
/// RETURNs the conflicting row (a plain `DO NOTHING ... UNION SELECT` would miss
/// a concurrent insert under READ COMMITTED — the loser's snapshot predates the
/// winner's commit and its SELECT returns zero rows → a spurious `RowNotFound`;
/// adversarial review, Sprint 7B). The `SET` is a self-assign — no real change,
/// just the lock + RETURNING.
async fn get_or_create_system(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    world: Uuid,
    currency: &str,
) -> Result<Uuid, Fail> {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO accounts (id, world_id, owner_kind, currency) \
         VALUES ($1, $2, 'system', $3) \
         ON CONFLICT (world_id, currency) WHERE owner_kind = 'system' \
         DO UPDATE SET currency = EXCLUDED.currency \
         RETURNING id",
    )
    .bind(new_id())
    .bind(world)
    .bind(currency)
    .fetch_one(&mut **tx)
    .await?;
    Ok(id)
}

/// A character's wallet for `currency`, created on first touch (a deposit is the
/// usual first touch). Race-safe the same way as [`get_or_create_system`].
async fn get_or_create_wallet(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    world: Uuid,
    character: Uuid,
    currency: &str,
) -> Result<Uuid, Fail> {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO accounts (id, world_id, owner_kind, owner_character, currency) \
         VALUES ($1, $2, 'character', $3, $4) \
         ON CONFLICT (world_id, owner_character, currency) WHERE owner_kind = 'character' \
         DO UPDATE SET currency = EXCLUDED.currency \
         RETURNING id",
    )
    .bind(new_id())
    .bind(world)
    .bind(character)
    .bind(currency)
    .fetch_one(&mut **tx)
    .await?;
    Ok(id)
}

// ── deposit ──────────────────────────────────────────────────────────────────

/// Deposit outcome. `credited` is the wallet owner to notify (incoming money,
/// §10.5 item 8) — `None` on an idempotent replay so the caller doesn't
/// double-notify.
pub struct DepositOutcome {
    pub credited: Option<Uuid>,
    pub amount: i64,
    pub state: String,
    pub fresh: bool,
}

/// `POST .../exchange` deposit (§10.5 item 4): credit `character`'s wallet from
/// the world's `system` account. Idempotent on the bridge-chosen `exchange_id`.
/// Auto-creates the system account and the wallet on first touch.
pub async fn deposit(
    pool: &PgPool,
    world: Uuid,
    exchange_id: &str,
    character: Uuid,
    amount: i64,
    currency: &str,
) -> Result<DepositOutcome, Fail> {
    if amount <= 0 {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    let mut tx = world_tx(pool, world).await?;

    // Idempotency: a re-sent deposit returns the stored result, moving nothing.
    // A truly-simultaneous duplicate that races past this SELECT unique-violates
    // on the exchanges PK INSERT below and rolls the whole tx back (no money
    // moved) — the bridge retries and lands here. (Same conservation-safe pattern
    // as transfers_idem in part A.)
    let existing: Option<(Uuid, i64, String)> = sqlx::query_as(
        "SELECT character_id, amount, state FROM exchanges WHERE world_id = $1 AND id = $2",
    )
    .bind(world)
    .bind(exchange_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((stored_char, stored_amount, state)) = existing {
        // Reusing an id for a *different* deposit is a bridge bug — reject it
        // rather than return a wrong-but-"success" ack (symmetry with
        // withdraw_confirm; adversarial review, Sprint 7B).
        if stored_char != character || stored_amount != amount {
            return Err(Fail::Code(ErrCode::Invalid));
        }
        tx.commit().await?;
        return Ok(DepositOutcome {
            credited: None,
            amount: stored_amount,
            state,
            fresh: false,
        });
    }

    // The character must exist in this world (the wallet FK would otherwise fail
    // with an opaque internal). Explicit world filter — correct with or without
    // RLS on `characters`.
    let ch: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM characters WHERE id = $1 AND world_id = $2")
            .bind(character)
            .bind(world)
            .fetch_optional(&mut *tx)
            .await?;
    if ch.is_none() {
        return Err(Fail::Code(ErrCode::NotFound));
    }

    let system = get_or_create_system(&mut tx, world, currency).await?;
    let wallet = get_or_create_wallet(&mut tx, world, character, currency).await?;

    // Lock both accounts with the SAME id-ordered idiom every other money op uses
    // (`IN($a,$b) ORDER BY id FOR UPDATE`) — deadlock-free against a concurrent
    // transfer/capture/withdraw_confirm on the same pair, without relying on the
    // implicit "system id < wallet id" ordering (adversarial review, Sprint 7B).
    // The system account is shared across a world's deposits, so they serialize on
    // it — fine at exchange frequency. System may run negative (the CHECK exempts
    // it), so no available check; a frozen system halts deposits.
    let rows: Vec<(Uuid, Option<OffsetDateTime>)> = sqlx::query_as(
        "SELECT id, frozen_at FROM accounts WHERE id IN ($1, $2) ORDER BY id FOR UPDATE",
    )
    .bind(system)
    .bind(wallet)
    .fetch_all(&mut *tx)
    .await?;
    if rows
        .iter()
        .find(|r| r.0 == system)
        .and_then(|r| r.1)
        .is_some()
    {
        return Err(Fail::Code(ErrCode::Conflict)); // frozen system
    }

    sqlx::query("UPDATE accounts SET balance = balance - $2 WHERE id = $1")
        .bind(system)
        .bind(amount)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE accounts SET balance = balance + $2 WHERE id = $1")
        .bind(wallet)
        .bind(amount)
        .execute(&mut *tx)
        .await?;
    // 'deposit' kind (not 'transfer') is what the reconcile cross-check keys on.
    sqlx::query(
        "INSERT INTO transfers (id, world_id, from_account, to_account, amount, kind, client_uuid) \
         VALUES ($1, $2, $3, $4, $5, 'deposit', NULL)",
    )
    .bind(new_id())
    .bind(world)
    .bind(system)
    .bind(wallet)
    .bind(amount)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO exchanges (world_id, id, character_id, amount, direction, state, hold_id) \
         VALUES ($1, $2, $3, $4, 'deposit', 'done', NULL)",
    )
    .bind(world)
    .bind(exchange_id)
    .bind(character)
    .bind(amount)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(DepositOutcome {
        credited: Some(character),
        amount,
        state: "done".into(),
        fresh: true,
    })
}

// ── withdraw (leg 1, WS) ─────────────────────────────────────────────────────

/// `ledger.withdraw` (§10.5 item 4), leg 1: reserve `amount` on the caller's
/// wallet with a hold and open a `pending_confirm` exchange. Returns the
/// Core-minted `exchange_id` the client relays to the bridge. No wallet, or
/// insufficient available balance → Conflict; frozen wallet → Conflict.
pub async fn withdraw(
    pool: &PgPool,
    world: Uuid,
    character: Uuid,
    currency: &str,
    amount: i64,
) -> Result<String, Fail> {
    if amount <= 0 {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    let mut tx = world_tx(pool, world).await?;
    // Lock the wallet FOR UPDATE before reading held_sum — the same invariant
    // part A's hold path relies on (available-check race-free under the row lock).
    let row: Option<(Uuid, i64, Option<OffsetDateTime>)> = sqlx::query_as(
        "SELECT id, balance, frozen_at FROM accounts \
         WHERE owner_character = $1 AND currency = $2 AND owner_kind = 'character' FOR UPDATE",
    )
    .bind(character)
    .bind(currency)
    .fetch_optional(&mut *tx)
    .await?;
    // No wallet = no funds to withdraw — same wire result as insufficient.
    let (wallet, balance, frozen) = row.ok_or(Fail::Code(ErrCode::Conflict))?;
    if frozen.is_some() {
        return Err(Fail::Code(ErrCode::Conflict));
    }
    if balance - held_sum(&mut tx, wallet).await? < amount {
        return Err(Fail::Code(ErrCode::Conflict)); // insufficient available
    }

    let hold_id = new_id();
    sqlx::query(
        "INSERT INTO holds (id, world_id, account_id, amount, state, expires_at) \
         VALUES ($1, $2, $3, $4, 'held', now() + make_interval(secs => $5))",
    )
    .bind(hold_id)
    .bind(world)
    .bind(wallet)
    .bind(amount)
    .bind(WITHDRAW_HOLD_SECS as f64)
    .execute(&mut *tx)
    .await?;

    // Core-minted exchange id (a uuid string), returned to the client → bridge.
    let exchange_id = new_id().to_string();
    sqlx::query(
        "INSERT INTO exchanges (world_id, id, character_id, amount, direction, state, hold_id) \
         VALUES ($1, $2, $3, $4, 'withdraw', 'pending_confirm', $5)",
    )
    .bind(world)
    .bind(&exchange_id)
    .bind(character)
    .bind(amount)
    .bind(hold_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(exchange_id)
}

// ── withdraw_confirm (leg 2, HTTP) ───────────────────────────────────────────

/// Confirm outcome (idempotent). `credited_amount` is echoed for the bridge.
pub struct ConfirmOutcome {
    pub state: String,
    pub amount: i64,
}

/// `POST .../exchange` withdraw_confirm (§10.5 item 4), leg 2: the bridge has
/// credited the framework bank, so settle the wallet's hold to `system` (wallet
/// → system, transfer `kind='withdraw'`) and mark the exchange `done`. Idempotent
/// (a re-confirm returns the stored result). `character_id`/`amount` in the
/// request must match the stored exchange (→ Invalid). Expired/unknown → Conflict/
/// NotFound; frozen wallet → Conflict.
pub async fn withdraw_confirm(
    pool: &PgPool,
    world: Uuid,
    exchange_id: &str,
    character: Uuid,
    amount: i64,
    currency: &str,
) -> Result<ConfirmOutcome, Fail> {
    let mut tx = world_tx(pool, world).await?;
    let ex: Option<(i64, String, Option<Uuid>, Uuid)> = sqlx::query_as(
        "SELECT amount, state, hold_id, character_id FROM exchanges \
         WHERE world_id = $1 AND id = $2 AND direction = 'withdraw' FOR UPDATE",
    )
    .bind(world)
    .bind(exchange_id)
    .fetch_optional(&mut *tx)
    .await?;
    let (ex_amount, state, hold_id, ex_char) = ex.ok_or(Fail::Code(ErrCode::NotFound))?;
    // The bridge's confirm must describe the same exchange Core recorded.
    if ex_char != character || ex_amount != amount {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    match state.as_str() {
        "done" => {
            // Already settled — idempotent replay.
            tx.commit().await?;
            return Ok(ConfirmOutcome {
                state,
                amount: ex_amount,
            });
        }
        "pending_confirm" => {}                         // proceed
        _ => return Err(Fail::Code(ErrCode::Conflict)), // expired
    }
    let hold_id = hold_id.ok_or_else(|| {
        Fail::Internal(anyhow::anyhow!("pending_confirm withdraw with no hold_id"))
    })?;

    let system = get_or_create_system(&mut tx, world, currency).await?;

    // Capture the hold: held → captured, then move the reserved amount wallet →
    // system. Lock the hold, then both accounts id-ordered (deadlock-free).
    let hold: Option<(Uuid, i64, String)> =
        sqlx::query_as("SELECT account_id, amount, state FROM holds WHERE id = $1 FOR UPDATE")
            .bind(hold_id)
            .fetch_optional(&mut *tx)
            .await?;
    let (wallet, hold_amount, hold_state) =
        hold.ok_or_else(|| Fail::Internal(anyhow::anyhow!("withdraw hold vanished")))?;
    let next = fsm::apply(parse_hold(&hold_state), Action::Capture)
        .map_err(|()| Fail::Code(ErrCode::Conflict))?;

    let rows: Vec<(Uuid, Option<OffsetDateTime>)> = sqlx::query_as(
        "SELECT id, frozen_at FROM accounts WHERE id IN ($1, $2) ORDER BY id FOR UPDATE",
    )
    .bind(wallet)
    .bind(system)
    .fetch_all(&mut *tx)
    .await?;
    if rows.len() != 2 {
        return Err(Fail::Internal(anyhow::anyhow!(
            "withdraw_confirm: wallet/system account missing"
        )));
    }
    // A frozen wallet (the debit source) OR a frozen system blocks the settle:
    // a reconciliation drift-freeze must halt ALL exchange flow, not just deposits
    // — withdraw_confirm settling INTO a frozen system would keep moving money
    // under an active integrity incident (adversarial review, Sprint 7B; deposit
    // already honors the frozen system).
    if rows.iter().any(|r| r.1.is_some()) {
        return Err(Fail::Code(ErrCode::Conflict));
    }

    sqlx::query("UPDATE holds SET state = $2 WHERE id = $1")
        .bind(hold_id)
        .bind(hold_str(next))
        .execute(&mut *tx)
        .await?;
    // Held funds were never debited (available-math excluded them), so debit now;
    // balance ≥ held amount by the invariant, so the CHECK never fires.
    sqlx::query("UPDATE accounts SET balance = balance - $2 WHERE id = $1")
        .bind(wallet)
        .bind(hold_amount)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE accounts SET balance = balance + $2 WHERE id = $1")
        .bind(system)
        .bind(hold_amount)
        .execute(&mut *tx)
        .await?;
    // 'withdraw' kind (not 'capture') — what the reconcile cross-check keys on.
    sqlx::query(
        "INSERT INTO transfers (id, world_id, from_account, to_account, amount, kind, client_uuid) \
         VALUES ($1, $2, $3, $4, $5, 'withdraw', NULL)",
    )
    .bind(new_id())
    .bind(world)
    .bind(wallet)
    .bind(system)
    .bind(hold_amount)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE exchanges SET state = 'done' WHERE world_id = $1 AND id = $2")
        .bind(world)
        .bind(exchange_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(ConfirmOutcome {
        state: "done".into(),
        amount: ex_amount,
    })
}

// ── journal (bridge reconciliation feed) ─────────────────────────────────────

/// One exchange in the journal (`GET .../exchange?since`).
#[derive(serde::Serialize)]
pub struct ExchangeItem {
    pub id: String,
    pub character_id: Uuid,
    pub amount: i64,
    pub direction: String,
    pub state: String,
    pub created_at: String,
}

/// `GET .../exchange?since&limit` (§10.5 item 4): the world's exchange journal for
/// the bridge's reconciliation, oldest-first, from `since` inclusive.
///
/// ponytail: `created_at >= since` (inclusive) keyset, not a full compound cursor.
/// The bridge advances `since` to the last row's `created_at` and dedups by `id`
/// (which it holds — it chose deposit ids, Core returns withdraw ids), so
/// re-reading the boundary microsecond is safe and no row is ever skipped. The
/// only failure mode — a full page sharing one microsecond stalling progress —
/// can't happen at bridge exchange frequency. Add a compound `(created_at, id)`
/// cursor if that ever changes.
pub async fn journal(
    pool: &PgPool,
    world: Uuid,
    since: OffsetDateTime,
    limit: i64,
) -> Result<Vec<ExchangeItem>, Fail> {
    let limit = limit.clamp(1, 500);
    let mut tx = world_tx(pool, world).await?;
    let rows: Vec<(String, Uuid, i64, String, String, OffsetDateTime)> = sqlx::query_as(
        "SELECT id, character_id, amount, direction, state, created_at FROM exchanges \
         WHERE created_at >= $1 ORDER BY created_at ASC, id ASC LIMIT $2",
    )
    .bind(since)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows
        .into_iter()
        .map(
            |(id, character_id, amount, direction, state, created_at)| ExchangeItem {
                id,
                character_id,
                amount,
                direction,
                state,
                created_at: rfc3339(created_at),
            },
        )
        .collect())
}

// ── reconciliation cross-check ───────────────────────────────────────────────

/// Exchange cross-check for `store::reconcile` (§10.5 item 7). Σ(done exchanges)
/// per direction must equal the matching system-account transfer legs — the
/// distinct 'deposit'/'withdraw' kinds, which only the exchange paths ever write.
/// On any mismatch, freeze every `system` account (halting exchange flow) and
/// count it. Runs inside reconcile's tx + advisory lock. Returns frozen ids.
pub(super) async fn cross_check(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    world: Uuid,
) -> anyhow::Result<Vec<Uuid>> {
    let (dep_ex, dep_tr, wdr_ex, wdr_tr): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT \
           COALESCE((SELECT SUM(amount) FROM exchanges WHERE direction = 'deposit' AND state = 'done'), 0)::bigint, \
           COALESCE((SELECT SUM(amount) FROM transfers WHERE kind = 'deposit'), 0)::bigint, \
           COALESCE((SELECT SUM(amount) FROM exchanges WHERE direction = 'withdraw' AND state = 'done'), 0)::bigint, \
           COALESCE((SELECT SUM(amount) FROM transfers WHERE kind = 'withdraw'), 0)::bigint",
    )
    .fetch_one(&mut **tx)
    .await?;
    if dep_ex == dep_tr && wdr_ex == wdr_tr {
        return Ok(Vec::new());
    }
    let frozen: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE accounts SET frozen_at = now() \
         WHERE owner_kind = 'system' AND frozen_at IS NULL RETURNING id",
    )
    .fetch_all(&mut **tx)
    .await?;
    counter!("opn_ledger_drift_total").increment(1);
    tracing::error!(
        %world, dep_ex, dep_tr, wdr_ex, wdr_tr,
        "exchange cross-check drift — froze system account(s)"
    );
    Ok(frozen)
}
