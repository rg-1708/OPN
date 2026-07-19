//! Sprint 9 property tests — channels seq gaplessness + idempotency (roadmap
//! Sprint 9 item 1, OPN-CORE.md §15). Generalizes the Sprint 3 concurrency-seq
//! test into a generative one: a random stream of sends, where client_uuids are
//! drawn from a small pool so duplicates recur, must satisfy —
//!   * per-channel `seq` is a gapless, dup-free `1..=distinct` (one seq per
//!     distinct client_uuid);
//!   * identical client_uuids produce identical `(message_id, seq)` acks and
//!     exactly one row (idempotency), with `deduped` set on all but the first;
//!   * `last_seq == distinct == count(*)` (the deduped attempts never bump seq).
//!
//! A concurrent variant runs the same stream across many tasks — the channel row
//! lock is the serialization point that must keep the above true under races.
//!
//! Same generator-loop shape as `prop_ledger.rs`: proptest generates the value
//! (`new_tree().current()`), the async test executes it against real Postgres,
//! `deterministic()` makes a red run reproduce, no auto-shrink (print-on-fail).

mod common;

use common::{app_pool, seed_world_tenant};
use opn_core::infra::db::world_tx;
use opn_core::primitives::channels::store;
use opn_core::primitives::identity;
use proptest::strategy::{Strategy, ValueTree};
use proptest::test_runner::TestRunner;
use serde_json::json;
use sqlx::PgPool;
use std::collections::HashMap;
use uuid::Uuid;

/// Distinct client_uuid slots in the pool. Fewer slots than sends → collisions
/// (the idempotency path) recur across a case.
const K: usize = 6;

fn cases() -> usize {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(16)
}

/// A stream of client_uuid pool-indices (0..K), length 0..40.
fn keys_strategy() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(0..K as u8, 0..40)
}

async fn last_seq(app: &PgPool, world: Uuid, channel: Uuid) -> i64 {
    let mut tx = world_tx(app, world).await.expect("tx");
    let s = sqlx::query_scalar("SELECT last_seq FROM channels WHERE id = $1")
        .bind(channel)
        .fetch_one(&mut *tx)
        .await
        .expect("last_seq");
    tx.commit().await.expect("commit");
    s
}

async fn message_count(app: &PgPool, world: Uuid, channel: Uuid) -> i64 {
    let mut tx = world_tx(app, world).await.expect("tx");
    let c = sqlx::query_scalar("SELECT count(*) FROM messages WHERE channel_id = $1")
        .bind(channel)
        .fetch_one(&mut *tx)
        .await
        .expect("count");
    tx.commit().await.expect("commit");
    c
}

/// Fresh world + sender + a solo group channel to hammer.
async fn seed_channel(admin: &PgPool, app: &PgPool) -> (Uuid, Uuid, Uuid) {
    let (world, tenant, _key) = seed_world_tenant(admin).await;
    let sender = identity::mint_session(app, tenant, world, "sender", None, 600)
        .await
        .expect("mint sender")
        .identity
        .character_id;
    let channel = store::create_group(app, world, sender, None, &[])
        .await
        .expect("create solo channel");
    (world, sender, channel)
}

/// After a case, assert `last_seq == count(*) == distinct` and the assigned seqs
/// are exactly `1..=distinct`.
async fn assert_gapless(app: &PgPool, world: Uuid, channel: Uuid, seqs: &[i64], ctx: &str) {
    let distinct = seqs.len() as i64;
    assert_eq!(
        last_seq(app, world, channel).await,
        distinct,
        "{ctx}: last_seq"
    );
    assert_eq!(
        message_count(app, world, channel).await,
        distinct,
        "{ctx}: row count == distinct client_uuids"
    );
    let mut sorted = seqs.to_vec();
    sorted.sort_unstable();
    let expected: Vec<i64> = (1..=distinct).collect();
    assert_eq!(sorted, expected, "{ctx}: seqs must be gapless 1..=distinct");
}

// ═══════════════════════════════════════════════════════════════════════════
// Sequential: dedup acks are identical, seq is gapless.
// ═══════════════════════════════════════════════════════════════════════════

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn channels_prop_seq_and_dedup_sequential(admin: PgPool) {
    let app = app_pool(&admin, 8).await;
    let mut runner = TestRunner::deterministic();
    let strat = keys_strategy();

    for case in 0..cases() {
        let keys = strat.new_tree(&mut runner).expect("gen keys").current();
        let (world, sender, channel) = seed_channel(&admin, &app).await;
        // A distinct real client_uuid per pool slot, minted fresh for this case.
        let uuids: Vec<Uuid> = (0..K).map(|_| Uuid::now_v7()).collect();
        // First ack seen per slot → later sends of the same slot must match it.
        let mut first: HashMap<u8, (Uuid, i64)> = HashMap::new();

        for &k in &keys {
            let out = store::send_message(
                &app,
                world,
                sender,
                channel,
                uuids[k as usize],
                &json!({ "text": "m" }),
            )
            .await
            .expect("send");
            match first.get(&k) {
                Some(&(mid, seq)) => {
                    assert!(out.deduped, "case {case}: repeat client_uuid must dedupe");
                    assert_eq!(out.message_id, mid, "case {case}: dedup message_id");
                    assert_eq!(out.seq, seq, "case {case}: dedup seq");
                }
                None => {
                    assert!(!out.deduped, "case {case}: first send must not dedupe");
                    first.insert(k, (out.message_id, out.seq));
                }
            }
        }

        let seqs: Vec<i64> = first.values().map(|&(_, s)| s).collect();
        assert_gapless(
            &app,
            world,
            channel,
            &seqs,
            &format!("case {case} keys={keys:?}"),
        )
        .await;
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Concurrent: the same stream across 8 tasks — the row lock keeps seq gapless
// and dedup consistent under races.
// ═══════════════════════════════════════════════════════════════════════════

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn channels_prop_seq_and_dedup_concurrent(admin: PgPool) {
    let app = app_pool(&admin, 16).await;
    let mut runner = TestRunner::deterministic();
    let strat = keys_strategy();

    const CASES: usize = 6;
    for case in 0..CASES.min(cases()) {
        let keys = strat.new_tree(&mut runner).expect("gen keys").current();
        let (world, sender, channel) = seed_channel(&admin, &app).await;
        let uuids: Vec<Uuid> = (0..K).map(|_| Uuid::now_v7()).collect();

        // One task per send; each returns (slot, message_id, seq).
        let mut handles = Vec::new();
        for &k in &keys {
            let app = app.clone();
            let cu = uuids[k as usize];
            handles.push(tokio::spawn(async move {
                let out =
                    store::send_message(&app, world, sender, channel, cu, &json!({ "text": "m" }))
                        .await
                        .expect("send");
                (k, out.message_id, out.seq)
            }));
        }
        let mut results = Vec::new();
        for h in handles {
            results.push(h.await.expect("task"));
        }

        // Every send sharing a slot must share one (message_id, seq).
        let mut by_slot: HashMap<u8, (Uuid, i64)> = HashMap::new();
        for &(slot, mid, seq) in &results {
            match by_slot.get(&slot) {
                Some(&(m0, s0)) => {
                    assert_eq!(mid, m0, "case {case}: slot {slot} message_id diverged");
                    assert_eq!(seq, s0, "case {case}: slot {slot} seq diverged");
                }
                None => {
                    by_slot.insert(slot, (mid, seq));
                }
            }
        }

        let seqs: Vec<i64> = by_slot.values().map(|&(_, s)| s).collect();
        assert_gapless(
            &app,
            world,
            channel,
            &seqs,
            &format!("case {case} keys={keys:?}"),
        )
        .await;
    }
}
