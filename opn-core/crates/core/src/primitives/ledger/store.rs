//! ledger SQL (OPN-CORE.md §10.5). Flat async fns over the pool; `mod.rs` does
//! validation + the post-commit notify, `fsm.rs` owns the hold transition logic.
//! Every query is world-scoped by `world_tx` (RLS).
//!
//! Two load-bearing choices:
//!   * **Deadlock-free locking.** A transfer/capture locks both accounts with
//!     `... WHERE id IN ($f,$t) ORDER BY id FOR UPDATE` — the one id-order, so two
//!     opposing concurrent transfers can never deadlock (§10.5). The `CHECK
//!     (balance >= 0 …)` is the backstop, not the mechanism.
//!   * **The reconciliation invariant.** An account is born at 0 and money moves
//!     ONLY via `transfers` rows, so `balance == Σ(to==id) − Σ(from==id)` holds
//!     for every account. `reconcile` recomputes exactly that and freezes any
//!     drift; the concurrency battery asserts the same equality. One source of
//!     truth, shared by prod and test.

use contracts::types::TransferItem;
use contracts::ErrCode;
use metrics::counter;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use super::fsm::{self, Action, HoldState};
use crate::infra::auth::Identity;
use crate::infra::cursor::{self, Cursor, Page};
use crate::infra::db::world_tx;
use crate::infra::ids::new_id;
use crate::infra::timefmt::rfc3339;
use crate::primitives::Fail;

// ── text ↔ enum (the DB stores lowercase text; fsm.rs carries the enum) ──

pub(super) fn hold_str(s: HoldState) -> &'static str {
    match s {
        HoldState::Held => "held",
        HoldState::Captured => "captured",
        HoldState::Released => "released",
    }
}
pub(super) fn parse_hold(s: &str) -> HoldState {
    match s {
        "captured" => HoldState::Captured,
        "released" => HoldState::Released,
        // Unknown/legacy rows read as held — the neutral default.
        _ => HoldState::Held,
    }
}

/// One locked account row (id, owner_character, currency, balance, frozen_at).
/// `owner_kind` is implied: `owner_character` is `Some` iff it is a character
/// wallet, `None` for the system account.
type AcctRow = (Uuid, Option<Uuid>, String, i64, Option<OffsetDateTime>);

/// `ledger.transfer` result. `to_owner` is the destination's character owner (the
/// notify target — `None` for a system destination); `fresh` is false on an
/// idempotent replay so the handler does not double-notify.
pub struct TransferOutcome {
    pub transfer_id: Uuid,
    pub from_balance: i64,
    pub to_owner: Option<Uuid>,
    pub amount: i64,
    pub fresh: bool,
}

/// `ledger.transfer` (§10.5): move `amount` from `from` to `to`, deadlock-free.
/// `from` must belong to `actor` (Forbidden otherwise). Idempotent on
/// `(from, client_uuid)`. Missing account → NotFound; frozen source or
/// insufficient available balance → Conflict; self-transfer or currency mismatch
/// → Invalid.
pub async fn transfer(
    pool: &PgPool,
    world: Uuid,
    actor: Uuid,
    from: Uuid,
    to: Uuid,
    amount: i64,
    client_uuid: Uuid,
) -> Result<TransferOutcome, Fail> {
    // `amount > 0` and self-transfer are guarded here too (not only in the
    // handler), so a future direct caller — the part-B exchange — can't reach the
    // debit path with a bad amount (defense-in-depth on a money fn).
    if from == to || amount <= 0 {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    let mut tx = world_tx(pool, world).await?;

    // Idempotency first (§10.5): a retry returns the stored transfer, moving
    // nothing. Scoped to accounts the actor OWNS (the join), so this fast path
    // cannot leak a non-owned account's balance ahead of the ownership check — a
    // non-owner's `(from, client_uuid)` simply misses here and falls through to
    // the locked path, which acks `Forbidden` before any INSERT (adversarial
    // review, Sprint 7A). The `transfers_idem` unique index is the concurrent
    // backstop: a truly-simultaneous duplicate that races past this SELECT
    // unique-violates on INSERT and rolls the whole tx back (no partial money
    // movement) — the client retries and hits this hit path. Sequential retries
    // (the common case) land here directly.
    let existing: Option<(Uuid, i64)> = sqlx::query_as(
        "SELECT t.id, a.balance FROM transfers t JOIN accounts a ON a.id = t.from_account \
         WHERE t.from_account = $1 AND t.client_uuid = $2 AND a.owner_character = $3",
    )
    .bind(from)
    .bind(client_uuid)
    .bind(actor)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((transfer_id, from_balance)) = existing {
        tx.commit().await?;
        return Ok(TransferOutcome {
            transfer_id,
            from_balance,
            to_owner: None,
            amount,
            fresh: false,
        });
    }

    // Lock both accounts in a fixed id order → no deadlock (§10.5).
    let rows: Vec<AcctRow> = sqlx::query_as(
        "SELECT id, owner_character, currency, balance, frozen_at \
         FROM accounts WHERE id IN ($1, $2) ORDER BY id FOR UPDATE",
    )
    .bind(from)
    .bind(to)
    .fetch_all(&mut *tx)
    .await?;
    if rows.len() != 2 {
        // One or both accounts missing/RLS-hidden.
        return Err(Fail::Code(ErrCode::NotFound));
    }
    let from_row = rows
        .iter()
        .find(|r| r.0 == from)
        .ok_or(Fail::Code(ErrCode::NotFound))?;
    let to_row = rows
        .iter()
        .find(|r| r.0 == to)
        .ok_or(Fail::Code(ErrCode::NotFound))?;

    if from_row.1 != Some(actor) {
        return Err(Fail::Code(ErrCode::Forbidden)); // only the owner may debit
    }
    if from_row.4.is_some() {
        return Err(Fail::Code(ErrCode::Conflict)); // frozen source (§10.5)
    }
    if from_row.2 != to_row.2 {
        return Err(Fail::Code(ErrCode::Invalid)); // no cross-currency value creation
    }
    if from_row.3 - held_sum(&mut tx, from).await? < amount {
        return Err(Fail::Code(ErrCode::Conflict)); // insufficient available balance
    }

    sqlx::query("UPDATE accounts SET balance = balance - $2 WHERE id = $1")
        .bind(from)
        .bind(amount)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE accounts SET balance = balance + $2 WHERE id = $1")
        .bind(to)
        .bind(amount)
        .execute(&mut *tx)
        .await?;
    let transfer_id = new_id();
    sqlx::query(
        "INSERT INTO transfers (id, world_id, from_account, to_account, amount, kind, client_uuid) \
         VALUES ($1, $2, $3, $4, $5, 'transfer', $6)",
    )
    .bind(transfer_id)
    .bind(world)
    .bind(from)
    .bind(to)
    .bind(amount)
    .bind(client_uuid)
    .execute(&mut *tx)
    .await?;

    let from_balance = from_row.3 - amount;
    let to_owner = to_row.1;
    tx.commit().await?;
    Ok(TransferOutcome {
        transfer_id,
        from_balance,
        to_owner,
        amount,
        fresh: true,
    })
}

/// Σ of a source account's active (`held`) holds — the amount excluded from
/// spendable balance. `available = balance − this`.
///
/// **Invariant this relies on:** every caller reads `held_sum` while holding the
/// account row `FOR UPDATE`, and every hold-writer (`hold`) takes that same row
/// lock before inserting — so no hold can be committed on the account between the
/// available-check and the debit. A future primitive that inserts a `holds` row
/// without first locking the account would reintroduce a check-vs-debit race.
pub(super) async fn held_sum(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    account: Uuid,
) -> Result<i64, Fail> {
    let held: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(amount), 0)::bigint FROM holds WHERE account_id = $1 AND state = 'held'",
    )
    .bind(account)
    .fetch_one(&mut **tx)
    .await?;
    Ok(held)
}

/// `ledger.hold` (§10.5): reserve `amount` of the caller's own `account` for
/// `expires_in_secs`, without moving it. Same available-check as a debit; the
/// hold counts against available until captured or released. Owner-only; frozen
/// or insufficient → Conflict; missing → NotFound.
pub async fn hold(
    pool: &PgPool,
    world: Uuid,
    actor: Uuid,
    account: Uuid,
    amount: i64,
    expires_in_secs: i64,
) -> Result<Uuid, Fail> {
    // Guarded here too (not only in the handler), for a future direct caller.
    if amount <= 0 {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    let mut tx = world_tx(pool, world).await?;
    let row: Option<(Option<Uuid>, i64, Option<OffsetDateTime>)> = sqlx::query_as(
        "SELECT owner_character, balance, frozen_at FROM accounts WHERE id = $1 FOR UPDATE",
    )
    .bind(account)
    .fetch_optional(&mut *tx)
    .await?;
    let (owner, balance, frozen) = row.ok_or(Fail::Code(ErrCode::NotFound))?;
    if owner != Some(actor) {
        return Err(Fail::Code(ErrCode::Forbidden));
    }
    if frozen.is_some() {
        return Err(Fail::Code(ErrCode::Conflict));
    }
    if balance - held_sum(&mut tx, account).await? < amount {
        return Err(Fail::Code(ErrCode::Conflict));
    }
    let hold_id = new_id();
    sqlx::query(
        "INSERT INTO holds (id, world_id, account_id, amount, state, expires_at) \
         VALUES ($1, $2, $3, $4, 'held', now() + make_interval(secs => $5))",
    )
    .bind(hold_id)
    .bind(world)
    .bind(account)
    .bind(amount)
    .bind(expires_in_secs as f64)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(hold_id)
}

/// `ledger.capture` result: the settling transfer + the destination's character
/// owner (notify target) and the amount.
pub struct CaptureOutcome {
    pub transfer_id: Uuid,
    pub to_owner: Option<Uuid>,
    pub amount: i64,
}

/// `ledger.capture { hold_id, to }` (§10.5): settle a held reservation, moving
/// the amount from the holding account to `to`. Held funds were never debited at
/// hold time (available-math already excluded them), so capture debits now — the
/// balance is guaranteed ≥ the held amount, so the `CHECK` never fires. FSM: held
/// → captured (terminal replay → Conflict). Only the holding account's owner may
/// capture; frozen source or currency mismatch → Conflict/Invalid.
pub async fn capture(
    pool: &PgPool,
    world: Uuid,
    actor: Uuid,
    hold_id: Uuid,
    to: Uuid,
) -> Result<CaptureOutcome, Fail> {
    let mut tx = world_tx(pool, world).await?;
    // Fetch the hold + its account's owner in one locked read and check ownership
    // BEFORE the FSM/self checks (mirrors `release`) — otherwise a non-owner could
    // learn a hold's state (held vs already-settled) from the error code before
    // authz runs (adversarial review, Sprint 7A). `owner_character` is immutable,
    // so reading it here (account not yet `FOR UPDATE`) is safe.
    // A hold that backs a pending withdraw exchange is off-limits to the public
    // capture/release API — only `withdraw_confirm`/`expire_holds` may move it.
    // Otherwise a character could capture their own withdraw's reservation
    // elsewhere, leaving the exchange `pending_confirm` for the bridge to confirm
    // after crediting the framework bank → money created (adversarial review,
    // Sprint 7B). Filtered out, so it reads as a nonexistent hold.
    let hold: Option<(Uuid, i64, String, Option<Uuid>)> = sqlx::query_as(
        "SELECT h.account_id, h.amount, h.state, a.owner_character \
         FROM holds h JOIN accounts a ON a.id = h.account_id \
         WHERE h.id = $1 \
           AND NOT EXISTS (SELECT 1 FROM exchanges e \
             WHERE e.hold_id = h.id AND e.state = 'pending_confirm') \
         FOR UPDATE OF h",
    )
    .bind(hold_id)
    .fetch_optional(&mut *tx)
    .await?;
    let (account, amount, state, owner) = hold.ok_or(Fail::Code(ErrCode::NotFound))?;
    if owner != Some(actor) {
        return Err(Fail::Code(ErrCode::Forbidden)); // only the hold owner may capture
    }
    let next = fsm::apply(parse_hold(&state), Action::Capture)
        .map_err(|()| Fail::Code(ErrCode::Conflict))?;
    if account == to {
        return Err(Fail::Code(ErrCode::Invalid)); // capturing to self is just a release
    }

    // Lock both accounts id-ordered (deadlock-free), like transfer.
    let rows: Vec<AcctRow> = sqlx::query_as(
        "SELECT id, owner_character, currency, balance, frozen_at \
         FROM accounts WHERE id IN ($1, $2) ORDER BY id FOR UPDATE",
    )
    .bind(account)
    .bind(to)
    .fetch_all(&mut *tx)
    .await?;
    if rows.len() != 2 {
        return Err(Fail::Code(ErrCode::NotFound));
    }
    let from_row = rows
        .iter()
        .find(|r| r.0 == account)
        .ok_or(Fail::Code(ErrCode::NotFound))?;
    let to_row = rows
        .iter()
        .find(|r| r.0 == to)
        .ok_or(Fail::Code(ErrCode::NotFound))?;
    // Ownership already checked on the hold's account above.
    if from_row.4.is_some() {
        return Err(Fail::Code(ErrCode::Conflict)); // frozen source
    }
    if from_row.2 != to_row.2 {
        return Err(Fail::Code(ErrCode::Invalid)); // currency mismatch
    }

    sqlx::query("UPDATE holds SET state = $2 WHERE id = $1")
        .bind(hold_id)
        .bind(hold_str(next))
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE accounts SET balance = balance - $2 WHERE id = $1")
        .bind(account)
        .bind(amount)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE accounts SET balance = balance + $2 WHERE id = $1")
        .bind(to)
        .bind(amount)
        .execute(&mut *tx)
        .await?;
    let transfer_id = new_id();
    sqlx::query(
        "INSERT INTO transfers (id, world_id, from_account, to_account, amount, kind, client_uuid) \
         VALUES ($1, $2, $3, $4, $5, 'capture', NULL)",
    )
    .bind(transfer_id)
    .bind(world)
    .bind(account)
    .bind(to)
    .bind(amount)
    .execute(&mut *tx)
    .await?;

    let to_owner = to_row.1;
    tx.commit().await?;
    Ok(CaptureOutcome {
        transfer_id,
        to_owner,
        amount,
    })
}

/// `ledger.release { hold_id }` (§10.5): free a held reservation without moving
/// money (the funds were never debited). FSM: held → released (terminal replay →
/// Conflict). Owner-only.
pub async fn release(pool: &PgPool, world: Uuid, actor: Uuid, hold_id: Uuid) -> Result<(), Fail> {
    let mut tx = world_tx(pool, world).await?;
    // Same exchange-backing guard as `capture`: a pending-withdraw hold is not
    // releasable via the public API (adversarial review, Sprint 7B).
    let row: Option<(Option<Uuid>, String)> = sqlx::query_as(
        "SELECT a.owner_character, h.state \
         FROM holds h JOIN accounts a ON a.id = h.account_id \
         WHERE h.id = $1 \
           AND NOT EXISTS (SELECT 1 FROM exchanges e \
             WHERE e.hold_id = h.id AND e.state = 'pending_confirm') \
         FOR UPDATE OF h",
    )
    .bind(hold_id)
    .fetch_optional(&mut *tx)
    .await?;
    let (owner, state) = row.ok_or(Fail::Code(ErrCode::NotFound))?;
    if owner != Some(actor) {
        return Err(Fail::Code(ErrCode::Forbidden));
    }
    let next = fsm::apply(parse_hold(&state), Action::Release)
        .map_err(|()| Fail::Code(ErrCode::Conflict))?;
    sqlx::query("UPDATE holds SET state = $2 WHERE id = $1")
        .bind(hold_id)
        .bind(hold_str(next))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct TransferRow {
    id: Uuid,
    from_account: Uuid,
    to_account: Uuid,
    amount: i64,
    kind: String,
    created_at: OffsetDateTime,
}

/// `GET /v1/ledger/history?cursor&limit` (§10.5): every transfer touching one of
/// the caller's own accounts (either leg), newest-first on the shared cursor
/// idiom (CDR-7).
pub async fn history(
    pool: &PgPool,
    who: &Identity,
    cursor: Option<Cursor>,
    limit: i64,
) -> Result<Page<TransferItem>, Fail> {
    let limit = limit.clamp(1, 100) as usize;
    let (cur_ts, cur_id) = match &cursor {
        Some(c) => (Some(c.ts), c.id),
        None => (None, Uuid::nil()),
    };
    let mut tx = world_tx(pool, who.world_id).await?;
    // The caller's account ids (usually one wallet). RLS already scopes to world.
    let accts: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM accounts WHERE owner_character = $1")
        .bind(who.character_id)
        .fetch_all(&mut *tx)
        .await?;
    let rows: Vec<TransferRow> = sqlx::query_as(
        "SELECT id, from_account, to_account, amount, kind, created_at FROM transfers \
         WHERE (from_account = ANY($1) OR to_account = ANY($1)) \
           AND ($2::timestamptz IS NULL OR (created_at, id) < ($2, $3)) \
         ORDER BY created_at DESC, id DESC LIMIT $4",
    )
    .bind(&accts)
    .bind(cur_ts)
    .bind(cur_id)
    .bind(limit as i64 + 1)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    let paged = cursor::page(rows, limit, |r| (r.created_at, r.id));
    Ok(Page {
        items: paged
            .items
            .into_iter()
            .map(|r| TransferItem {
                id: r.id,
                from_account: r.from_account,
                to_account: r.to_account,
                amount: r.amount,
                kind: r.kind,
                created_at: rfc3339(r.created_at),
            })
            .collect(),
        next_cursor: paged.next_cursor,
    })
}

/// Nightly reconciliation (§10.5), the invariant in one statement: freeze every
/// account whose stored `balance` disagrees with the net of its transfers
/// (`Σ to==id − Σ from==id`). Shared by the janitor task and the concurrency
/// battery — one source of truth. Idempotent (the `frozen_at IS NULL` guard), so
/// running it repeatedly during the reconcile hour re-stamps nothing. Under the
/// per-task advisory lock (rule 7). Returns the frozen account ids.
pub async fn reconcile(pool: &PgPool, world: Uuid) -> anyhow::Result<Vec<Uuid>> {
    let mut tx = world_tx(pool, world).await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('janitor:ledger_reconcile'))")
        .execute(&mut *tx)
        .await?;
    let mut frozen: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE accounts a SET frozen_at = now() \
         WHERE a.frozen_at IS NULL \
           AND a.balance <> ( \
             COALESCE((SELECT SUM(amount) FROM transfers WHERE to_account = a.id), 0) \
             - COALESCE((SELECT SUM(amount) FROM transfers WHERE from_account = a.id), 0) \
           ) \
         RETURNING a.id",
    )
    .fetch_all(&mut *tx)
    .await?;
    // Exchange cross-check (§10.5 item 7): Σ(done exchanges) vs the matching
    // system-account transfer legs (the distinct 'deposit'/'withdraw' kinds). A
    // mismatch = an exchange row that lost or gained its money leg — corruption
    // the per-account balance recompute above cannot see, because a missing
    // leg + missing money is self-consistent on balances. Freezes the system
    // account so exchange flow halts until a human looks. Runs in the same tx /
    // advisory lock as the balance freeze.
    frozen.extend(super::exchange::cross_check(&mut tx, world).await?);
    tx.commit().await?;
    if !frozen.is_empty() {
        counter!("opn_ledger_drift_total").increment(frozen.len() as u64);
        tracing::error!(
            count = frozen.len(),
            %world,
            "ledger reconciliation froze drifted accounts — silent corruption detected"
        );
    }
    Ok(frozen)
}

/// Janitor (§10.5 item 6): release every `held` hold past its expiry, returning
/// the character owner + amount of each so the janitor can `silent`-notify. System
/// holds (no character owner) are released but yield no notify target. Idempotent
/// under the per-task advisory lock; the `state = 'held'` guard makes a re-run a
/// no-op (rule 7).
pub async fn expire_holds(pool: &PgPool, world: Uuid) -> anyhow::Result<Vec<(Uuid, i64)>> {
    let mut tx = world_tx(pool, world).await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('janitor:ledger_expire_holds'))")
        .execute(&mut *tx)
        .await?;
    // Expire the withdraw exchanges backing to-be-released holds FIRST, then
    // release the holds. This locks exchange rows before hold rows — the SAME
    // order `withdraw_confirm` uses (exchange FOR UPDATE, then hold FOR UPDATE) —
    // so a confirm racing a hold's expiry can't deadlock against this sweep
    // (adversarial review, Sprint 7B). The subquery is a plain snapshot read (no
    // lock); both statements share the `state='held' AND expires_at < now()`
    // predicate, so they act on the same set. Idempotent (the `pending_confirm`
    // guard).
    sqlx::query(
        "UPDATE exchanges SET state = 'expired' \
         WHERE state = 'pending_confirm' \
           AND hold_id IN (SELECT id FROM holds WHERE state = 'held' AND expires_at < now())",
    )
    .execute(&mut *tx)
    .await?;
    let released: Vec<(Option<Uuid>, i64)> = sqlx::query_as(
        "UPDATE holds h SET state = 'released' \
         FROM accounts a \
         WHERE h.account_id = a.id AND h.state = 'held' AND h.expires_at < now() \
         RETURNING a.owner_character, h.amount",
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(released
        .into_iter()
        .filter_map(|(owner, amount)| owner.map(|c| (c, amount)))
        .collect())
}
