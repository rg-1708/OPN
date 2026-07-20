//! Sprint 7 part A ledger tests (OPN-CORE.md §10.5): money that cannot be
//! created, destroyed, or double-spent. The hold FSM has its own unit tests in
//! `ledger/fsm.rs`; here we drive the store/handler seam — transfers, holds,
//! captures, releases, the reconciliation invariant, hold-expiry, RLS — plus a
//! concurrency battery and an opposing-transfer storm (the deadlock-free lock
//! order, and money conservation under contention).
//!
//! Mostly direct-primitive tests (à la `calls.rs`/`directory.rs`): a `world_tx`
//! DB read is a sharper assertion for money than draining WS events. One WS wire
//! test proves the dispatch arm + rate class actually route.

mod common;

use common::ws::{connect_and_auth, mint_token, spawn_server};
use common::{app_pool, seed_world_tenant, test_config, test_state};
use contracts::ErrCode;
use opn_core::infra::auth::Identity;
use opn_core::infra::cursor;
use opn_core::infra::db::world_tx;
use opn_core::primitives::{identity, ledger, Fail};
use opn_core::state::AppState;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

// ── fixture + helpers ─────────────────────────────────────────────────────

/// AppState (RLS-on `opn_app` pool, live Redis) + two characters, each with one
/// funded wallet, plus the world's system account. Wallets are funded FROM the
/// system account via a real transfer row, so the ledger invariant (balance == Σ
/// transfers) holds from the start — the same way exchange deposits will fund
/// them in part B.
struct Fx {
    state: AppState,
    world: Uuid,
    a: Identity,
    b: Identity,
    system: Uuid,
    acct_a: Uuid,
    acct_b: Uuid,
}

async fn fixture(admin: &PgPool) -> Fx {
    let (world, tenant, _key) = seed_world_tenant(admin).await;
    let pool = app_pool(admin, 16).await;
    let state = test_state(pool, test_config()).await;
    let a = identity::mint_session(&state.pg, tenant, world, "alice", None, 600)
        .await
        .expect("mint alice")
        .identity;
    let b = identity::mint_session(&state.pg, tenant, world, "bob", None, 600)
        .await
        .expect("mint bob")
        .identity;
    let system = seed_account(&state, world, None, "cred").await;
    let acct_a = seed_account(&state, world, Some(a.character_id), "cred").await;
    let acct_b = seed_account(&state, world, Some(b.character_id), "cred").await;
    fund(&state, world, system, acct_a, 1000).await;
    fund(&state, world, system, acct_b, 500).await;
    Fx {
        state,
        world,
        a,
        b,
        system,
        acct_a,
        acct_b,
    }
}

/// Seed a zero-balance account. `owner = None` → the system account.
async fn seed_account(state: &AppState, world: Uuid, owner: Option<Uuid>, currency: &str) -> Uuid {
    let id = Uuid::now_v7();
    let kind = if owner.is_some() {
        "character"
    } else {
        "system"
    };
    let mut tx = world_tx(&state.pg, world).await.expect("tx");
    sqlx::query(
        "INSERT INTO accounts (id, world_id, owner_kind, owner_character, currency, balance) \
         VALUES ($1, $2, $3, $4, $5, 0)",
    )
    .bind(id)
    .bind(world)
    .bind(kind)
    .bind(owner)
    .bind(currency)
    .execute(&mut *tx)
    .await
    .expect("seed account");
    tx.commit().await.expect("commit");
    id
}

/// Fund `to` from the system account, preserving the ledger invariant (every
/// balance move is a transfer row). Test-only — prod funds via exchange deposit
/// (Sprint 7 part B).
async fn fund(state: &AppState, world: Uuid, system: Uuid, to: Uuid, amount: i64) {
    let mut tx = world_tx(&state.pg, world).await.expect("tx");
    sqlx::query("UPDATE accounts SET balance = balance - $2 WHERE id = $1")
        .bind(system)
        .bind(amount)
        .execute(&mut *tx)
        .await
        .expect("debit system");
    sqlx::query("UPDATE accounts SET balance = balance + $2 WHERE id = $1")
        .bind(to)
        .bind(amount)
        .execute(&mut *tx)
        .await
        .expect("credit");
    sqlx::query(
        "INSERT INTO transfers (id, world_id, from_account, to_account, amount, kind) \
         VALUES ($1, $2, $3, $4, $5, 'transfer')",
    )
    .bind(Uuid::now_v7())
    .bind(world)
    .bind(system)
    .bind(to)
    .bind(amount)
    .execute(&mut *tx)
    .await
    .expect("genesis transfer");
    tx.commit().await.expect("commit");
}

async fn balance(state: &AppState, world: Uuid, account: Uuid) -> i64 {
    let mut tx = world_tx(&state.pg, world).await.expect("tx");
    let b: i64 = sqlx::query_scalar("SELECT balance FROM accounts WHERE id = $1")
        .bind(account)
        .fetch_one(&mut *tx)
        .await
        .expect("balance");
    tx.commit().await.expect("commit");
    b
}

async fn freeze(state: &AppState, world: Uuid, account: Uuid) {
    let mut tx = world_tx(&state.pg, world).await.expect("tx");
    sqlx::query("UPDATE accounts SET frozen_at = now() WHERE id = $1")
        .bind(account)
        .execute(&mut *tx)
        .await
        .expect("freeze");
    tx.commit().await.expect("commit");
}

fn hold_id_of(ack: &serde_json::Value) -> Uuid {
    ack["hold_id"]
        .as_str()
        .unwrap_or_else(|| panic!("no hold_id: {ack}"))
        .parse()
        .expect("hold_id uuid")
}

fn code_of(r: Result<serde_json::Value, Fail>) -> ErrCode {
    match r {
        Err(Fail::Code(c)) => c,
        other => panic!("expected an error code, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// REQUIRED — the named tests the coverage match-test points at.
// ═══════════════════════════════════════════════════════════════════════════

/// `ledger.transfer` happy path + the three deliberate rejections (§10.5):
/// insufficient available balance, missing account, frozen source. A failed
/// transfer moves nothing.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn transfer_happy_insufficient_frozen_missing(admin: PgPool) {
    let fx = fixture(&admin).await;

    // Happy: alice(1000) → bob, 300.
    let ack = ledger::transfer(&fx.state, &fx.a, fx.acct_a, fx.acct_b, 300, Uuid::now_v7())
        .await
        .expect("transfer ok");
    assert_eq!(
        ack["balance"],
        json!(700),
        "source's new balance in the ack"
    );
    assert_eq!(
        balance(&fx.state, fx.world, fx.acct_a).await,
        700,
        "source debited"
    );
    assert_eq!(
        balance(&fx.state, fx.world, fx.acct_b).await,
        800,
        "dest credited"
    );

    // Insufficient available balance → conflict, no movement.
    let r = ledger::transfer(
        &fx.state,
        &fx.a,
        fx.acct_a,
        fx.acct_b,
        10_000,
        Uuid::now_v7(),
    )
    .await;
    assert_eq!(code_of(r), ErrCode::Conflict, "insufficient funds");
    assert_eq!(
        balance(&fx.state, fx.world, fx.acct_a).await,
        700,
        "no debit on a failed transfer"
    );

    // Missing destination account → not_found.
    let r = ledger::transfer(
        &fx.state,
        &fx.a,
        fx.acct_a,
        Uuid::now_v7(),
        10,
        Uuid::now_v7(),
    )
    .await;
    assert_eq!(code_of(r), ErrCode::NotFound, "missing account");

    // Frozen source → conflict (outgoing blocked).
    freeze(&fx.state, fx.world, fx.acct_a).await;
    let r = ledger::transfer(&fx.state, &fx.a, fx.acct_a, fx.acct_b, 10, Uuid::now_v7()).await;
    assert_eq!(code_of(r), ErrCode::Conflict, "frozen source can't send");
}

/// `admin unfreeze` closes the reconciliation loop (Sprint 7 item 7 / Sprint 11
/// item 5): a frozen account rejects outgoing ops, the CLI's owner-role
/// `unfreeze_account` thaws exactly it, then it can send again — and a second
/// thaw is a no-op (0 rows). This is the recovery path the frozen-account runbook
/// documents; the roadmap named it a Sprint 7 exit criterion but the CLI never
/// shipped until now.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn admin_unfreeze_clears_freeze(admin: PgPool) {
    let fx = fixture(&admin).await;

    freeze(&fx.state, fx.world, fx.acct_a).await;
    let r = ledger::transfer(&fx.state, &fx.a, fx.acct_a, fx.acct_b, 100, Uuid::now_v7()).await;
    assert_eq!(code_of(r), ErrCode::Conflict, "frozen source can't send");

    // The owner-role thaw the CLI runs (admin pool = migrate role, RLS-bypassing).
    let n = opn_core::admin::unfreeze_account(&admin, fx.world, fx.acct_a)
        .await
        .expect("unfreeze");
    assert_eq!(n, 1, "exactly the frozen account thawed");

    ledger::transfer(&fx.state, &fx.a, fx.acct_a, fx.acct_b, 100, Uuid::now_v7())
        .await
        .expect("thawed account can send again");

    let n2 = opn_core::admin::unfreeze_account(&admin, fx.world, fx.acct_a)
        .await
        .expect("unfreeze again");
    assert_eq!(n2, 0, "already-thawed account is a no-op");
}

/// Ownership (only the owner may debit) + idempotency (a `client_uuid` retry
/// returns the original ack and debits exactly once) (§10.5).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn transfer_ownership_and_idempotency(admin: PgPool) {
    let fx = fixture(&admin).await;

    // Bob tries to debit Alice's account → forbidden.
    let r = ledger::transfer(&fx.state, &fx.b, fx.acct_a, fx.acct_b, 10, Uuid::now_v7()).await;
    assert_eq!(code_of(r), ErrCode::Forbidden, "only the owner may debit");

    // Same client_uuid twice → identical transfer id, one debit.
    let key = Uuid::now_v7();
    let first = ledger::transfer(&fx.state, &fx.a, fx.acct_a, fx.acct_b, 100, key)
        .await
        .expect("first");
    let second = ledger::transfer(&fx.state, &fx.a, fx.acct_a, fx.acct_b, 100, key)
        .await
        .expect("idempotent retry");
    assert_eq!(
        first["transfer_id"], second["transfer_id"],
        "retry returns the original transfer id"
    );
    assert_eq!(
        balance(&fx.state, fx.world, fx.acct_a).await,
        900,
        "debited once, not twice"
    );

    let mut tx = world_tx(&fx.state.pg, fx.world).await.expect("tx");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM transfers WHERE from_account = $1")
        .bind(fx.acct_a)
        .fetch_one(&mut *tx)
        .await
        .expect("count");
    tx.commit().await.expect("commit");
    assert_eq!(n, 1, "exactly one transfer row despite the retry");
}

/// Hold → capture / release lifecycle (§10.5): a held reservation excludes its
/// amount from available balance, capture settles it to a destination, release
/// frees it, and both terminal states reject a replay (the FSM).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn hold_capture_release_lifecycle(admin: PgPool) {
    let fx = fixture(&admin).await;

    // Hold 400 on alice (balance 1000 → available 600).
    let hold_id = hold_id_of(
        &ledger::hold(&fx.state, &fx.a, fx.acct_a, 400, 3600)
            .await
            .expect("hold"),
    );

    // A transfer that would dip below the reserve → conflict.
    let r = ledger::transfer(&fx.state, &fx.a, fx.acct_a, fx.acct_b, 700, Uuid::now_v7()).await;
    assert_eq!(
        code_of(r),
        ErrCode::Conflict,
        "the 400 hold is excluded from available"
    );

    // Within available (600) succeeds; the hold still reserves 400.
    ledger::transfer(&fx.state, &fx.a, fx.acct_a, fx.acct_b, 500, Uuid::now_v7())
        .await
        .expect("within available");
    assert_eq!(
        balance(&fx.state, fx.world, fx.acct_a).await,
        500,
        "moved 500, hold still holds 400"
    );

    // Capture the hold to bob: held → captured, alice debited 400 now.
    let cap = ledger::capture(&fx.state, &fx.a, hold_id, fx.acct_b)
        .await
        .expect("capture");
    assert!(cap["transfer_id"].is_string(), "capture yields a transfer");
    assert_eq!(
        balance(&fx.state, fx.world, fx.acct_a).await,
        100,
        "captured 400 debited now"
    );
    // bob: 500 (funded) + 500 (transfer) + 400 (capture).
    assert_eq!(
        balance(&fx.state, fx.world, fx.acct_b).await,
        1400,
        "bob credited transfer + capture"
    );
    // Capture writes a `kind='capture'` transfer row, so it stays conservation-
    // consistent: reconcile freezes nothing after a capture.
    {
        let mut tx = world_tx(&fx.state.pg, fx.world).await.expect("tx");
        let caps: i64 = sqlx::query_scalar("SELECT count(*) FROM transfers WHERE kind = 'capture'")
            .fetch_one(&mut *tx)
            .await
            .expect("count captures");
        tx.commit().await.expect("commit");
        assert_eq!(caps, 1, "one capture transfer row written");
    }
    assert!(
        ledger::store::reconcile(&fx.state.pg, fx.world)
            .await
            .expect("reconcile")
            .is_empty(),
        "capture is conservation-consistent — no drift"
    );

    // Re-capturing a captured hold → conflict (terminal).
    let r = ledger::capture(&fx.state, &fx.a, hold_id, fx.acct_b).await;
    assert_eq!(code_of(r), ErrCode::Conflict, "a captured hold is terminal");

    // Release path on a fresh hold: held → released, no money moves.
    let hold2 = hold_id_of(
        &ledger::hold(&fx.state, &fx.a, fx.acct_a, 50, 3600)
            .await
            .expect("hold2"),
    );
    ledger::release(&fx.state, &fx.a, hold2)
        .await
        .expect("release");
    assert_eq!(
        balance(&fx.state, fx.world, fx.acct_a).await,
        100,
        "release moves nothing"
    );

    // Re-releasing → conflict (terminal).
    assert!(
        matches!(
            ledger::release(&fx.state, &fx.a, hold2).await,
            Err(Fail::Code(ErrCode::Conflict))
        ),
        "a released hold is terminal"
    );
}

/// Concurrency battery + reconciliation (§10.5, the sprint's subtle-bug magnet):
/// 16 tasks × 25 transfers among 8 wallets, opposing directions included. The
/// id-ordered `FOR UPDATE` lock means no deadlock; money is conserved
/// (Σ balances == 0 incl. the system account); and `reconcile` — the SAME
/// invariant SQL prod uses — freezes nothing. Exit criterion: green 100 runs.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn concurrency_battery_conserves_and_reconciles(admin: PgPool) {
    let (world, tenant, _key) = seed_world_tenant(&admin).await;
    let pool = app_pool(&admin, 16).await;
    let state = test_state(pool, test_config()).await;
    let system = seed_account(&state, world, None, "cred").await;

    let mut ids = Vec::new();
    let mut accts = Vec::new();
    for i in 0..8 {
        let who = identity::mint_session(&state.pg, tenant, world, &format!("p{i}"), None, 600)
            .await
            .expect("mint")
            .identity;
        let acct = seed_account(&state, world, Some(who.character_id), "cred").await;
        fund(&state, world, system, acct, 1000).await;
        ids.push(who);
        accts.push(acct);
    }

    let mut handles = Vec::new();
    for task in 0..16u64 {
        let st = state.clone();
        let ids = ids.clone();
        let accts = accts.clone();
        handles.push(tokio::spawn(async move {
            let (mut oks, mut internals) = (0u32, 0u32);
            for iter in 0..25u64 {
                // Deterministic-but-varied pairs (incl. opposing i↔j). A distinct
                // client_uuid each time so idempotency never dedups. Each debit is
                // from account[i] as its owner ids[i].
                let i = ((task * 7 + iter * 13) % 8) as usize;
                let mut j = ((task * 11 + iter * 17 + 1) % 8) as usize;
                if j == i {
                    j = (j + 1) % 8;
                }
                let amount = ((task + iter) % 40 + 1) as i64;
                match ledger::transfer(&st, &ids[i], accts[i], accts[j], amount, Uuid::now_v7())
                    .await
                {
                    Ok(_) => oks += 1,
                    Err(Fail::Code(_)) => {} // insufficient — fine
                    Err(Fail::Internal(_)) => internals += 1, // a deadlock/error is a hard fail
                }
            }
            (oks, internals)
        }));
    }
    let (mut oks, mut internals) = (0u32, 0u32);
    for h in handles {
        let (o, n) = h.await.expect("task join");
        oks += o;
        internals += n;
    }
    assert_eq!(
        internals, 0,
        "no deadlock / internal errors under contention"
    );
    // The battery must actually MOVE money — else Σ==0 (true by construction) and
    // reconcile-clean (true when nothing moved) would pass VACUOUSLY even if every
    // transfer had failed. Assert a healthy fraction of the 400 succeeded, so the
    // conservation + reconcile checks below are exercised on real movement
    // (adversarial review, Sprint 7A keeper).
    assert!(
        oks > 200,
        "money must actually move under contention (oks={oks}/400)"
    );

    // Money is conserved: the whole ledger (8 wallets + system) sums to 0.
    let mut tx = world_tx(&state.pg, world).await.expect("tx");
    let total: i64 = sqlx::query_scalar("SELECT COALESCE(SUM(balance), 0)::bigint FROM accounts")
        .fetch_one(&mut *tx)
        .await
        .expect("sum balances");
    tx.commit().await.expect("commit");
    assert_eq!(total, 0, "money is conserved: Σ balances == 0");

    // Reconciliation (the same invariant) freezes nothing — no account drifted.
    let frozen = ledger::store::reconcile(&state.pg, world)
        .await
        .expect("reconcile");
    assert!(frozen.is_empty(), "no account drifted: {frozen:?}");
}

// ═══════════════════════════════════════════════════════════════════════════
// Reconciliation, holds-expiry, RLS, deadlock storm, CHECK, WS wiring.
// ═══════════════════════════════════════════════════════════════════════════

/// A → B and B → A, 200 iterations each, concurrently. The id-ordered `FOR
/// UPDATE` lock serializes opposing transfers rather than deadlocking. Amounts of
/// 1 so neither wallet is ever insufficient — an `Internal` here means a deadlock.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn opposing_transfer_storm_no_deadlock(admin: PgPool) {
    let fx = fixture(&admin).await;
    let (st1, a1, acct_a, acct_b) = (fx.state.clone(), fx.a.clone(), fx.acct_a, fx.acct_b);
    let ab = tokio::spawn(async move {
        let mut internals = 0u32;
        for _ in 0..200 {
            if let Err(Fail::Internal(_)) =
                ledger::transfer(&st1, &a1, acct_a, acct_b, 1, Uuid::now_v7()).await
            {
                internals += 1;
            }
        }
        internals
    });
    let (st2, b2, acct_a2, acct_b2) = (fx.state.clone(), fx.b.clone(), fx.acct_a, fx.acct_b);
    let ba = tokio::spawn(async move {
        let mut internals = 0u32;
        for _ in 0..200 {
            if let Err(Fail::Internal(_)) =
                ledger::transfer(&st2, &b2, acct_b2, acct_a2, 1, Uuid::now_v7()).await
            {
                internals += 1;
            }
        }
        internals
    });
    let (n1, n2) = tokio::join!(ab, ba);
    assert_eq!(
        n1.expect("ab") + n2.expect("ba"),
        0,
        "opposing transfers never deadlock"
    );
}

/// Reconciliation catches silent corruption (§10.5): a balance changed OUTSIDE a
/// transfer (a bug, a bad actor) drifts from its transfer-net, so `reconcile`
/// freezes exactly that account and its outgoing ops start rejecting. Idempotent.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn reconciliation_freezes_injected_corruption(admin: PgPool) {
    let fx = fixture(&admin).await;

    ledger::transfer(&fx.state, &fx.a, fx.acct_a, fx.acct_b, 100, Uuid::now_v7())
        .await
        .expect("transfer");
    assert!(
        ledger::store::reconcile(&fx.state.pg, fx.world)
            .await
            .expect("reconcile clean")
            .is_empty(),
        "a consistent ledger freezes nothing"
    );

    // Corrupt bob's balance out of band (not via a transfer).
    {
        let mut tx = world_tx(&fx.state.pg, fx.world).await.expect("tx");
        sqlx::query("UPDATE accounts SET balance = balance + 999 WHERE id = $1")
            .bind(fx.acct_b)
            .execute(&mut *tx)
            .await
            .expect("inject corruption");
        tx.commit().await.expect("commit");
    }

    let frozen = ledger::store::reconcile(&fx.state.pg, fx.world)
        .await
        .expect("reconcile");
    assert_eq!(frozen, vec![fx.acct_b], "the drifted account is frozen");

    // A frozen account rejects outgoing ops.
    let r = ledger::transfer(&fx.state, &fx.b, fx.acct_b, fx.acct_a, 1, Uuid::now_v7()).await;
    assert_eq!(code_of(r), ErrCode::Conflict, "frozen account can't send");

    // Idempotent: a second pass re-freezes nothing (frozen_at already set).
    assert!(
        ledger::store::reconcile(&fx.state.pg, fx.world)
            .await
            .expect("reconcile again")
            .is_empty(),
        "already-frozen accounts aren't re-stamped"
    );
}

/// The system account may run negative (it is the mint/sink); a character wallet
/// may not — the `CHECK (balance >= 0 OR owner_kind = 'system')` rejects it
/// (§10.5).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn negative_system_allowed_character_impossible(admin: PgPool) {
    let fx = fixture(&admin).await;

    // The system account already went negative funding both wallets.
    assert!(
        balance(&fx.state, fx.world, fx.system).await < 0,
        "system funded both wallets → negative balance allowed"
    );

    // A direct write pushing a character wallet negative is rejected by the CHECK.
    let mut tx = world_tx(&fx.state.pg, fx.world).await.expect("tx");
    let res = sqlx::query("UPDATE accounts SET balance = -1 WHERE id = $1")
        .bind(fx.acct_a)
        .execute(&mut *tx)
        .await;
    assert!(res.is_err(), "CHECK forbids a negative character balance");
    drop(tx); // aborted tx → rollback
}

/// Hold-expiry janitor (§10.5 item 6): a `held` hold past its expiry is released
/// by the sweep, its owner + amount returned for the silent notify, and the
/// reserved balance is freed. Idempotent.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn hold_expiry_releases(admin: PgPool) {
    let fx = fixture(&admin).await;

    let hold_id = hold_id_of(
        &ledger::hold(&fx.state, &fx.a, fx.acct_a, 200, 3600)
            .await
            .expect("hold"),
    );
    // Age it past its expiry directly.
    {
        let mut tx = world_tx(&fx.state.pg, fx.world).await.expect("tx");
        sqlx::query("UPDATE holds SET expires_at = now() - interval '1 minute' WHERE id = $1")
            .bind(hold_id)
            .execute(&mut *tx)
            .await
            .expect("age hold");
        tx.commit().await.expect("commit");
    }

    let released = ledger::store::expire_holds(&fx.state.pg, fx.world)
        .await
        .expect("expire");
    assert_eq!(
        released,
        vec![(fx.a.character_id, 200)],
        "expired hold released; owner + amount returned to notify"
    );

    // Reserve freed: a full-balance transfer now succeeds.
    ledger::transfer(&fx.state, &fx.a, fx.acct_a, fx.acct_b, 1000, Uuid::now_v7())
        .await
        .expect("hold no longer reserves");

    // Idempotent: nothing left to expire.
    assert!(
        ledger::store::expire_holds(&fx.state.pg, fx.world)
            .await
            .expect("again")
            .is_empty(),
        "already-released holds aren't re-swept"
    );
}

/// Cross-world RLS isolation (mirrors `calls.rs`/`directory.rs`): ledger rows in
/// world A are invisible under world B's tx. Raw unfiltered counts, so a zero can
/// only come from the policy; the owning-world count proves the rows exist.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn cross_world_rls_isolation(admin: PgPool) {
    let fx = fixture(&admin).await;
    ledger::transfer(&fx.state, &fx.a, fx.acct_a, fx.acct_b, 100, Uuid::now_v7())
        .await
        .expect("transfer");
    ledger::hold(&fx.state, &fx.a, fx.acct_a, 50, 3600)
        .await
        .expect("hold");
    let (world_b, _tenant_b, _key_b) = seed_world_tenant(&admin).await;

    for (table, count_sql) in [
        ("accounts", "SELECT count(*) FROM accounts"),
        ("transfers", "SELECT count(*) FROM transfers"),
        ("holds", "SELECT count(*) FROM holds"),
    ] {
        let mut tx_a = world_tx(&fx.state.pg, fx.world).await.expect("tx a");
        let in_a: i64 = sqlx::query_scalar(count_sql)
            .fetch_one(&mut *tx_a)
            .await
            .expect("count a");
        tx_a.commit().await.expect("commit a");
        assert!(in_a > 0, "{table}: owning world sees its rows");

        let mut tx_b = world_tx(&fx.state.pg, world_b).await.expect("tx b");
        let in_b: i64 = sqlx::query_scalar(count_sql)
            .fetch_one(&mut *tx_b)
            .await
            .expect("count b");
        tx_b.commit().await.expect("commit b");
        assert_eq!(in_b, 0, "{table}: cross-world read must be empty (RLS)");
    }
}

/// WS wire smoke: `ledger.transfer` routes through dispatch (rate class Money,
/// the arm, serde) and returns the ack shape. The logic is covered by the
/// direct-primitive tests above; this proves the wiring.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn transfer_over_ws(admin: PgPool) {
    let (world, tenant, _key) = seed_world_tenant(&admin).await;
    let pool = app_pool(&admin, 8).await;
    let state = test_state(pool, test_config()).await;
    let (token_a, a) = mint_token(&state.pg, tenant, world, "alice").await;
    let (_token_b, b) = mint_token(&state.pg, tenant, world, "bob").await;
    let system = seed_account(&state, world, None, "cred").await;
    let acct_a = seed_account(&state, world, Some(a.character_id), "cred").await;
    let acct_b = seed_account(&state, world, Some(b.character_id), "cred").await;
    fund(&state, world, system, acct_a, 500).await;

    let server = spawn_server(state).await;
    let mut ca = connect_and_auth(server.addr, &token_a).await;
    let ack = ca
        .cmd(json!({ "cmd": "ledger.transfer", "payload": {
            "from_account": acct_a,
            "to_account": acct_b,
            "amount": 200,
            "client_uuid": Uuid::now_v7(),
        }}))
        .await;
    assert_eq!(ack["ok"], json!(true), "transfer ack ok: {ack}");
    assert_eq!(
        ack["payload"]["balance"],
        json!(300),
        "new source balance in ack"
    );
}

/// `GET /v1/ledger/history` (§10.5): the cursor page walks the caller's own
/// transfers (both legs + funding) with no dup/skip, and NEVER shows a transfer
/// between two other characters. (adversarial review: this endpoint was wired but
/// untested.)
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn history_paginates_and_isolates(admin: PgPool) {
    let (world, tenant, _key) = seed_world_tenant(&admin).await;
    let pool = app_pool(&admin, 8).await;
    let state = test_state(pool, test_config()).await;
    let alice = identity::mint_session(&state.pg, tenant, world, "alice", None, 600)
        .await
        .expect("a")
        .identity;
    let bob = identity::mint_session(&state.pg, tenant, world, "bob", None, 600)
        .await
        .expect("b")
        .identity;
    let carol = identity::mint_session(&state.pg, tenant, world, "carol", None, 600)
        .await
        .expect("c")
        .identity;
    let system = seed_account(&state, world, None, "cred").await;
    let acct_a = seed_account(&state, world, Some(alice.character_id), "cred").await;
    let acct_b = seed_account(&state, world, Some(bob.character_id), "cred").await;
    let acct_c = seed_account(&state, world, Some(carol.character_id), "cred").await;
    fund(&state, world, system, acct_a, 1000).await; // touches acct_a (to leg)
    fund(&state, world, system, acct_b, 1000).await;
    fund(&state, world, system, acct_c, 1000).await;

    // alice on both legs (5 sends + 3 receives); carol→bob touches neither.
    for _ in 0..5 {
        ledger::transfer(&state, &alice, acct_a, acct_b, 10, Uuid::now_v7())
            .await
            .expect("a->b");
    }
    for _ in 0..3 {
        ledger::transfer(&state, &bob, acct_b, acct_a, 10, Uuid::now_v7())
            .await
            .expect("b->a");
    }
    let mut carol_ids = Vec::new();
    for _ in 0..2 {
        let ack = ledger::transfer(&state, &carol, acct_c, acct_b, 10, Uuid::now_v7())
            .await
            .expect("c->b");
        carol_ids.push(
            ack["transfer_id"]
                .as_str()
                .expect("transfer_id str")
                .parse::<Uuid>()
                .expect("transfer_id uuid"),
        );
    }

    // Walk with a tiny page size so pagination is actually exercised.
    let mut ids = Vec::new();
    let mut cur = None;
    loop {
        let page = ledger::store::history(&state.pg, &alice, cur.take(), 2)
            .await
            .expect("history");
        ids.extend(page.items.iter().map(|i| i.id));
        match page.next_cursor {
            Some(c) => cur = Some(cursor::decode(&c).expect("decode cursor")),
            None => break,
        }
    }

    let mut uniq = ids.clone();
    uniq.sort();
    uniq.dedup();
    assert_eq!(uniq.len(), ids.len(), "pages must not duplicate a row");
    // funding(system→a) + 5 (a→b) + 3 (b→a) = 9.
    assert_eq!(
        ids.len(),
        9,
        "alice sees exactly her own transfers (both legs)"
    );
    for cid in &carol_ids {
        assert!(!ids.contains(cid), "another character's transfer leaked");
    }
}

/// Input validation at the money boundary (§10.5): zero/negative amounts and a
/// nil `client_uuid` are rejected `Invalid` and move nothing. The nil-key check
/// is the Sprint-7A keeper — a nil idempotency key would silently trap the
/// account into one transfer forever.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn transfer_rejects_bad_amount_and_nil_key(admin: PgPool) {
    let fx = fixture(&admin).await;
    assert_eq!(
        code_of(ledger::transfer(&fx.state, &fx.a, fx.acct_a, fx.acct_b, 0, Uuid::now_v7()).await),
        ErrCode::Invalid,
        "zero amount"
    );
    assert_eq!(
        code_of(
            ledger::transfer(&fx.state, &fx.a, fx.acct_a, fx.acct_b, -100, Uuid::now_v7()).await
        ),
        ErrCode::Invalid,
        "negative amount"
    );
    assert_eq!(
        code_of(ledger::transfer(&fx.state, &fx.a, fx.acct_a, fx.acct_b, 10, Uuid::nil()).await),
        ErrCode::Invalid,
        "nil client_uuid rejected (would trap the account)"
    );
    assert_eq!(
        balance(&fx.state, fx.world, fx.acct_a).await,
        1000,
        "no debit on any rejected transfer"
    );
    assert_eq!(
        code_of(ledger::hold(&fx.state, &fx.a, fx.acct_a, 0, 3600).await),
        ErrCode::Invalid,
        "zero-amount hold rejected"
    );
}

/// Capture/transfer edges (§10.5): capture-to-self and cross-currency capture are
/// rejected `Invalid` (before the hold transitions); crediting INTO a frozen
/// account is allowed (freeze blocks outgoing only), but the frozen account can't
/// send. (adversarial review: these correct paths were untested.)
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn capture_and_transfer_edges(admin: PgPool) {
    let fx = fixture(&admin).await;
    let h = hold_id_of(
        &ledger::hold(&fx.state, &fx.a, fx.acct_a, 100, 3600)
            .await
            .expect("hold"),
    );
    // Capture to self → Invalid (it's just a release).
    assert_eq!(
        code_of(ledger::capture(&fx.state, &fx.a, h, fx.acct_a).await),
        ErrCode::Invalid,
        "capture to self"
    );
    // Cross-currency capture → Invalid: a 'gold' account, the hold is 'cred'.
    let acct_b_gold = seed_account(&fx.state, fx.world, Some(fx.b.character_id), "gold").await;
    assert_eq!(
        code_of(ledger::capture(&fx.state, &fx.a, h, acct_b_gold).await),
        ErrCode::Invalid,
        "cross-currency capture"
    );
    // Neither rejection transitioned the hold — still capturable to a valid dest.
    ledger::capture(&fx.state, &fx.a, h, fx.acct_b)
        .await
        .expect("valid capture still works after the rejected attempts");

    // Crediting into a FROZEN account is allowed (frozen blocks outgoing only).
    freeze(&fx.state, fx.world, fx.acct_b).await;
    let before = balance(&fx.state, fx.world, fx.acct_b).await;
    ledger::transfer(&fx.state, &fx.a, fx.acct_a, fx.acct_b, 50, Uuid::now_v7())
        .await
        .expect("credit into a frozen account is allowed");
    assert_eq!(
        balance(&fx.state, fx.world, fx.acct_b).await,
        before + 50,
        "frozen account still receives"
    );
    // But a frozen account cannot SEND.
    assert_eq!(
        code_of(ledger::transfer(&fx.state, &fx.b, fx.acct_b, fx.acct_a, 1, Uuid::now_v7()).await),
        ErrCode::Conflict,
        "frozen account can't send"
    );
}
