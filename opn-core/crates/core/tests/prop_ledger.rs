//! Sprint 9 property tests — ledger money conservation (roadmap Sprint 9 item 1,
//! OPN-CORE.md §15). Generated op sequences (transfer / hold / capture / release)
//! run against real Postgres; after each sequence the ledger invariants must
//! hold:
//!   1. per-account `balance == Σ(to==id) − Σ(from==id)` — the SAME `reconcile`
//!      SQL prod freezes on (no account drifted);
//!   2. `Σ balances == 0` — conservation (the system account may run negative);
//!   3. no character wallet is negative (the `CHECK` backstop, asserted directly);
//!   4. `available = balance − Σ held ≥ 0` for every account (holds never
//!      over-reserve).
//!
//! A concurrent variant splits transfers across 8 tasks and asserts only the
//! global invariants (1, 2) — the deadlock-free `FOR UPDATE … ORDER BY id` lock
//! order under contention, generalizing the Sprint 7 concurrency battery.
//!
//! proptest drives the *generation* here (`new_tree().current()` inside the async
//! `#[sqlx::test]`), not the sync `proptest!` runner — that keeps one DB pool and
//! plain `.await`, at the cost of automatic shrinking. On failure the whole op
//! sequence + case index prints, so a minimized regression test is added by hand
//! (the roadmap's "generative finds, deterministic remembers"). The runner is
//! `deterministic()` so a red CI reproduces locally byte-for-byte.
//! ponytail: no-shrink generator loop; add async-shrinking machinery only if a
//! real failure ever proves un-minimizable by hand.

mod common;

use common::{app_pool, seed_world_tenant, test_config, test_state};
use opn_core::infra::auth::Identity;
use opn_core::infra::db::world_tx;
use opn_core::primitives::{identity, ledger, Fail};
use opn_core::state::AppState;
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;
use sqlx::PgPool;
use uuid::Uuid;

const N_WALLETS: usize = 4;
const FUND: i64 = 1_000;

/// Local case count: `PROPTEST_CASES` (default 16; nightly sets 256).
fn cases() -> usize {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(16)
}

// ── generated ops (indices resolved to real ids at execution time) ──────────

#[derive(Debug, Clone)]
enum Op {
    /// Debit wallet `from` (its owner acts), credit account index `to`.
    Transfer { from: u8, to: u8, amount: i64 },
    /// Reserve `amount` on wallet `acct`.
    Hold { acct: u8, amount: i64 },
    /// Settle the live hold at index `hold` to account index `to`.
    Capture { hold: u8, to: u8 },
    /// Release the live hold at index `hold`.
    Release { hold: u8 },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0..N_WALLETS as u8, 0..=N_WALLETS as u8, 1i64..200)
            .prop_map(|(from, to, amount)| Op::Transfer { from, to, amount }),
        (0..N_WALLETS as u8, 1i64..200).prop_map(|(acct, amount)| Op::Hold { acct, amount }),
        (0..64u8, 0..=N_WALLETS as u8).prop_map(|(hold, to)| Op::Capture { hold, to }),
        (0..64u8).prop_map(|hold| Op::Release { hold }),
    ]
}

fn seq_strategy() -> impl Strategy<Value = Vec<Op>> {
    proptest::collection::vec(op_strategy(), 0..40)
}

fn transfers_strategy() -> impl Strategy<Value = Vec<(u8, u8, i64)>> {
    proptest::collection::vec((0..N_WALLETS as u8, 0..=N_WALLETS as u8, 1i64..200), 128)
}

// ── per-case world fixture ──────────────────────────────────────────────────

/// A fresh RLS-isolated world: the system account + `N_WALLETS` funded wallets,
/// each owned by a distinct character. `all[0]` is the system account, `all[1..]`
/// the wallets; `idents[i]` owns `all[i + 1]`.
struct World {
    world: Uuid,
    all: Vec<Uuid>,
    idents: Vec<Identity>,
}

async fn seed_world(admin: &PgPool, state: &AppState) -> World {
    let (world, tenant, _key) = seed_world_tenant(admin).await;
    let system = seed_account(state, world, None).await;
    let mut all = vec![system];
    let mut idents = Vec::new();
    for i in 0..N_WALLETS {
        let id = identity::mint_session(&state.pg, tenant, world, &format!("w{i}"), None, 600)
            .await
            .expect("mint wallet owner")
            .identity;
        let acct = seed_account(state, world, Some(id.character_id)).await;
        fund(state, world, system, acct, FUND).await;
        all.push(acct);
        idents.push(id);
    }
    World { world, all, idents }
}

async fn seed_account(state: &AppState, world: Uuid, owner: Option<Uuid>) -> Uuid {
    let id = Uuid::now_v7();
    let kind = if owner.is_some() {
        "character"
    } else {
        "system"
    };
    let mut tx = world_tx(&state.pg, world).await.expect("tx");
    sqlx::query(
        "INSERT INTO accounts (id, world_id, owner_kind, owner_character, currency, balance) \
         VALUES ($1, $2, $3, $4, 'cred', 0)",
    )
    .bind(id)
    .bind(world)
    .bind(kind)
    .bind(owner)
    .execute(&mut *tx)
    .await
    .expect("seed account");
    tx.commit().await.expect("commit");
    id
}

/// Genesis funding: a real system→wallet transfer row (keeps `balance == Σ
/// transfers` true from the start, the way exchange deposits fund wallets).
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
        .expect("credit wallet");
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

// ── invariant checks (world-scoped by RLS) ──────────────────────────────────

async fn sum_balances(state: &AppState, world: Uuid) -> i64 {
    let mut tx = world_tx(&state.pg, world).await.expect("tx");
    let s: i64 = sqlx::query_scalar("SELECT COALESCE(SUM(balance), 0)::bigint FROM accounts")
        .fetch_one(&mut *tx)
        .await
        .expect("sum balances");
    tx.commit().await.expect("commit");
    s
}

/// Count of *character wallets* violating a non-negative balance or `available ≥
/// 0`. The `system` account is excluded from both: it is the mint and runs
/// negative by design (`CHECK (balance >= 0 OR owner_kind = 'system')`), and it
/// never carries holds.
async fn violations(state: &AppState, world: Uuid) -> i64 {
    let mut tx = world_tx(&state.pg, world).await.expect("tx");
    let v: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM accounts a WHERE a.owner_kind = 'character' AND ( \
           a.balance < 0 \
           OR a.balance - COALESCE( \
                (SELECT SUM(amount) FROM holds WHERE account_id = a.id AND state = 'held'), 0 \
              ) < 0 \
         )",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("violations");
    tx.commit().await.expect("commit");
    v
}

/// Assert the full invariant set on a world after a sequence ran. `ctx` labels
/// the failing case in the panic message so it can be minimized by hand.
async fn assert_invariants(state: &AppState, world: Uuid, ctx: &str) {
    let frozen = ledger::store::reconcile(&state.pg, world)
        .await
        .expect("reconcile");
    assert!(
        frozen.is_empty(),
        "{ctx}: account(s) drifted from Σ-transfers invariant: {frozen:?}"
    );
    assert_eq!(
        sum_balances(state, world).await,
        0,
        "{ctx}: Σ balances != 0"
    );
    assert_eq!(
        violations(state, world).await,
        0,
        "{ctx}: a wallet went negative or available < 0"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Sequential: the full op mix (transfer/hold/capture/release), every invariant.
// ═══════════════════════════════════════════════════════════════════════════

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn ledger_prop_conserves_sequential(admin: PgPool) {
    let pool = app_pool(&admin, 8).await;
    let state = test_state(pool, test_config()).await;
    let mut runner = TestRunner::deterministic();
    let strat = seq_strategy();

    for case in 0..cases() {
        let ops = strat.new_tree(&mut runner).expect("gen ops").current();
        let w = seed_world(&admin, &state).await;
        // Live holds resolved by execution-time index: (hold_id, wallet index).
        let mut live: Vec<(Uuid, usize)> = Vec::new();

        for op in &ops {
            match *op {
                Op::Transfer { from, to, amount } => {
                    let fw = from as usize % N_WALLETS;
                    let to_acct = w.all[to as usize % w.all.len()];
                    let r = ledger::transfer(
                        &state,
                        &w.idents[fw],
                        w.all[fw + 1],
                        to_acct,
                        amount,
                        Uuid::now_v7(),
                    )
                    .await;
                    assert_no_internal(r, case, &ops, "transfer");
                }
                Op::Hold { acct, amount } => {
                    let aw = acct as usize % N_WALLETS;
                    match ledger::hold(&state, &w.idents[aw], w.all[aw + 1], amount, 3600).await {
                        Ok(v) => {
                            let hid: Uuid =
                                serde_json::from_value(v["hold_id"].clone()).expect("hold_id");
                            live.push((hid, aw));
                        }
                        Err(Fail::Code(_)) => {}
                        Err(Fail::Internal(e)) => {
                            panic!("case {case} ops={ops:?}: hold internal error: {e:?}")
                        }
                    }
                }
                Op::Capture { hold, to } => {
                    if live.is_empty() {
                        continue;
                    }
                    let idx = hold as usize % live.len();
                    let (hid, aw) = live.remove(idx);
                    let to_acct = w.all[to as usize % w.all.len()];
                    let r = ledger::capture(&state, &w.idents[aw], hid, to_acct).await;
                    assert_no_internal(r, case, &ops, "capture");
                }
                Op::Release { hold } => {
                    if live.is_empty() {
                        continue;
                    }
                    let idx = hold as usize % live.len();
                    let (hid, aw) = live.remove(idx);
                    let r = ledger::release(&state, &w.idents[aw], hid).await;
                    assert_no_internal(r, case, &ops, "release");
                }
            }
        }

        assert_invariants(&state, w.world, &format!("case {case} ops={ops:?}")).await;
    }
}

/// A `Fail::Internal` = a deadlock or DB error and is a hard bug; `Fail::Code` is
/// an expected domain rejection (insufficient / conflict / forbidden / invalid).
fn assert_no_internal<T>(r: Result<T, Fail>, case: usize, ops: &[Op], what: &str) {
    if let Err(Fail::Internal(e)) = r {
        panic!("case {case} ops={ops:?}: {what} internal error: {e:?}");
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Concurrent: transfers only, split across 8 tasks — deadlock-free lock order +
// conservation under contention (the global invariants).
// ═══════════════════════════════════════════════════════════════════════════

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn ledger_prop_conserves_concurrent(admin: PgPool) {
    let pool = app_pool(&admin, 16).await;
    let state = test_state(pool, test_config()).await;
    let mut runner = TestRunner::deterministic();
    let strat = transfers_strategy();

    const CASES: usize = 4; // each case runs 128 concurrent transfers — keep it small
    for case in 0..CASES.min(cases()) {
        let txns = strat.new_tree(&mut runner).expect("gen txns").current();
        let w = seed_world(&admin, &state).await;

        const TASKS: usize = 8;
        let mut buckets: Vec<Vec<(u8, u8, i64)>> = vec![Vec::new(); TASKS];
        for (i, t) in txns.iter().enumerate() {
            buckets[i % TASKS].push(*t);
        }

        let mut handles = Vec::new();
        for bucket in buckets {
            let state = state.clone();
            let all = w.all.clone();
            let idents = w.idents.clone();
            handles.push(tokio::spawn(async move {
                let (mut oks, mut internals) = (0u32, 0u32);
                for (from, to, amount) in bucket {
                    let fw = from as usize % N_WALLETS;
                    let to_acct = all[to as usize % all.len()];
                    match ledger::transfer(
                        &state,
                        &idents[fw],
                        all[fw + 1],
                        to_acct,
                        amount,
                        Uuid::now_v7(),
                    )
                    .await
                    {
                        Ok(_) => oks += 1,
                        Err(Fail::Code(_)) => {}
                        Err(Fail::Internal(_)) => internals += 1,
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
            "case {case}: deadlock/internal under contention"
        );
        assert!(
            oks > 24,
            "case {case}: too little money moved (oks={oks}/128)"
        );
        assert_invariants(&state, w.world, &format!("case {case} concurrent")).await;
    }
}
