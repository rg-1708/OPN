//! The channels invariants that are subtle enough to be the sprint's named
//! bug magnets (roadmap Sprint 3 test plan + exit criteria): per-channel `seq`
//! is gapless and monotonic under concurrent senders, idempotency holds under
//! concurrent identical `client_uuid`s and across month partitions, and
//! concurrent `open_direct` of one pair converges on a single channel.
//!
//! These drive the store layer directly (no WS) so the concurrency is real and
//! tight; the protocol-level happy paths live in `tests/channels.rs`. In
//! Sprint 9 the gapless-seq property becomes a proptest.

mod common;

use common::ws::{connect_and_auth, mint_full, spawn_server};
use common::{app_pool, seed_world_tenant, test_config, test_state};
use opn_core::infra::db::world_tx;
use opn_core::infra::ids::new_id;
use opn_core::primitives::channels::store;
use serde_json::json;
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

const EVT_WAIT: Duration = Duration::from_secs(2);
const SHORT: Duration = Duration::from_millis(400);

/// A solo group channel with `owner` as its only member — the simplest place
/// to hammer the send path.
async fn solo_channel(app: &PgPool, world: Uuid, owner: Uuid) -> Uuid {
    store::create_group(app, world, owner, None, &[])
        .await
        .expect("create solo channel")
}

async fn message_count(app: &PgPool, world: Uuid, channel: Uuid) -> i64 {
    let mut tx = world_tx(app, world).await.expect("world_tx");
    sqlx::query_scalar("SELECT count(*) FROM messages WHERE channel_id = $1")
        .bind(channel)
        .fetch_one(&mut *tx)
        .await
        .expect("count messages")
}

async fn last_seq(app: &PgPool, world: Uuid, channel: Uuid) -> i64 {
    let mut tx = world_tx(app, world).await.expect("world_tx");
    sqlx::query_scalar("SELECT last_seq FROM channels WHERE id = $1")
        .bind(channel)
        .fetch_one(&mut *tx)
        .await
        .expect("last_seq")
}

/// THE invariant: 16 concurrent senders × 50 messages into one channel produce
/// seqs that are a gapless, dup-free 1..=800. Each send uses a distinct
/// `client_uuid` so none dedupe. The channel row lock is the serialization
/// point that must make this hold.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn concurrent_senders_gapless(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 24).await;
    let (_t, m) = mint_full(&app, tenant, world, "sender").await;
    let sender = m.identity.character_id;
    let channel = solo_channel(&app, world, sender).await;

    const TASKS: usize = 16;
    const PER: usize = 50;

    let mut handles = Vec::with_capacity(TASKS);
    for _ in 0..TASKS {
        let app = app.clone();
        handles.push(tokio::spawn(async move {
            let mut seqs = Vec::with_capacity(PER);
            for _ in 0..PER {
                let out = store::send_message(
                    &app,
                    world,
                    sender,
                    channel,
                    new_id(),
                    &json!({ "text": "m" }),
                )
                .await
                .expect("send");
                assert!(!out.deduped, "distinct client_uuids must not dedupe");
                seqs.push(out.seq);
            }
            seqs
        }));
    }

    let mut all = Vec::with_capacity(TASKS * PER);
    for h in handles {
        all.extend(h.await.expect("task"));
    }
    all.sort_unstable();

    let expected: Vec<i64> = (1..=(TASKS * PER) as i64).collect();
    assert_eq!(all, expected, "seqs must be a gapless, dup-free 1..=N");
    assert_eq!(last_seq(&app, world, channel).await, (TASKS * PER) as i64);
    assert_eq!(
        message_count(&app, world, channel).await,
        (TASKS * PER) as i64
    );
}

/// An idempotent retry (same `client_uuid`) returns the original ack, writes no
/// second row, and does not advance `last_seq` (the deduped attempt rolls back
/// its seq bump).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn idempotent_retry_same_ack(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let (_t, m) = mint_full(&app, tenant, world, "sender").await;
    let sender = m.identity.character_id;
    let channel = solo_channel(&app, world, sender).await;

    let cu = new_id();
    let first = store::send_message(&app, world, sender, channel, cu, &json!({ "text": "hi" }))
        .await
        .expect("first send");
    assert!(!first.deduped);
    assert_eq!(first.seq, 1);

    let retry = store::send_message(&app, world, sender, channel, cu, &json!({ "text": "hi" }))
        .await
        .expect("retry send");
    assert!(retry.deduped, "same client_uuid must dedupe");
    assert_eq!(retry.message_id, first.message_id);
    assert_eq!(retry.seq, first.seq);

    assert_eq!(message_count(&app, world, channel).await, 1);
    assert_eq!(
        last_seq(&app, world, channel).await,
        1,
        "dedup must not bump seq"
    );
}

/// Two concurrent sends with the *same* `client_uuid`: exactly one row, both
/// callers get the same `(message_id, seq)`, and `seq` has no gap. This is the
/// race the post-lock idempotency check exists to close.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn concurrent_identical_client_uuid(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let (_t, m) = mint_full(&app, tenant, world, "sender").await;
    let sender = m.identity.character_id;
    let channel = solo_channel(&app, world, sender).await;

    let cu = new_id();
    let a = {
        let app = app.clone();
        tokio::spawn(async move {
            store::send_message(&app, world, sender, channel, cu, &json!({ "text": "x" })).await
        })
    };
    let b = {
        let app = app.clone();
        tokio::spawn(async move {
            store::send_message(&app, world, sender, channel, cu, &json!({ "text": "x" })).await
        })
    };
    let a = a.await.expect("task a").expect("send a");
    let b = b.await.expect("task b").expect("send b");

    assert_eq!(
        a.message_id, b.message_id,
        "same client_uuid → same message"
    );
    assert_eq!(a.seq, b.seq);
    assert!(a.deduped ^ b.deduped, "exactly one of the two is the dedup");
    assert_eq!(message_count(&app, world, channel).await, 1);
    assert_eq!(
        last_seq(&app, world, channel).await,
        1,
        "no gap from the deduped racer"
    );
}

/// Cross-partition idempotency (roadmap Sprint 3 item 2): a message sent last
/// month lives in a different partition, so the DB unique (which carries the
/// partition key) cannot catch a retry now. The store's index-backed pre-check
/// must still dedupe it.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn cross_partition_idempotency(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let (_t, m) = mint_full(&app, tenant, world, "sender").await;
    let sender = m.identity.character_id;
    let channel = solo_channel(&app, world, sender).await;

    let cu = new_id();
    let original = new_id();
    // Make last month's partition exist, then seed a message into it and set the
    // channel watermark as if that send happened last month.
    {
        let mut tx = world_tx(&app, world).await.expect("world_tx");
        sqlx::query("SELECT ensure_message_partition(now() - interval '1 month')")
            .execute(&mut *tx)
            .await
            .expect("ensure last-month partition");
        sqlx::query(
            "INSERT INTO messages \
               (id, world_id, channel_id, seq, sender_character, body, client_uuid, created_at) \
             VALUES ($1, $2, $3, 1, $4, $5, $6, now() - interval '1 month')",
        )
        .bind(original)
        .bind(world)
        .bind(channel)
        .bind(sender)
        .bind(json!({ "text": "last month" }))
        .bind(cu)
        .execute(&mut *tx)
        .await
        .expect("seed old-partition message");
        sqlx::query("UPDATE channels SET last_seq = 1 WHERE id = $1")
            .bind(channel)
            .execute(&mut *tx)
            .await
            .expect("bump last_seq");
        tx.commit().await.expect("commit seed");
    }

    // Retry the same client_uuid now (current-month partition) → must dedupe to
    // the last-month row, insert nothing, leave last_seq at 1.
    let retry = store::send_message(&app, world, sender, channel, cu, &json!({ "text": "now" }))
        .await
        .expect("retry send");
    assert!(retry.deduped, "cross-partition retry must dedupe");
    assert_eq!(retry.message_id, original);
    assert_eq!(retry.seq, 1);
    assert_eq!(message_count(&app, world, channel).await, 1);
    assert_eq!(last_seq(&app, world, channel).await, 1);
}

/// A deduped retry over the real WS handler fans out no second event.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn idempotent_retry_no_second_event(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 12).await;
    let (a_tok, _a) = mint_full(&app, tenant, world, "alice").await;
    let (_b_tok, b) = mint_full(&app, tenant, world, "bob").await;
    let b_number = b.character.number.clone().expect("bob has a number");

    let server = spawn_server(test_state(app.clone(), test_config()).await).await;

    // Alice opens the pair thread to Bob and subscribes to it herself.
    let mut alice = connect_and_auth(server.addr, &a_tok).await;
    let opened = alice
        .cmd(json!({ "cmd": "channels.open_direct", "payload": { "number": b_number } }))
        .await;
    assert_eq!(opened["ok"], json!(true), "open_direct: {opened}");
    let channel = opened["payload"]["channel_id"]
        .as_str()
        .expect("channel_id")
        .to_string();
    let sub = alice
        .cmd(json!({ "cmd": "sub", "payload": { "topic": format!("ch:{channel}") } }))
        .await;
    assert_eq!(sub["ok"], json!(true), "sub: {sub}");

    let cu = Uuid::now_v7().to_string();
    let send = json!({
        "cmd": "channels.send",
        "payload": { "channel_id": channel, "client_uuid": cu, "body": { "text": "hi" } }
    });

    let first = alice.cmd(send.clone()).await;
    assert_eq!(first["ok"], json!(true), "first send: {first}");
    let evt = alice.expect_evt(EVT_WAIT).await;
    assert_eq!(evt["evt"], json!("channels.message"));
    assert_eq!(evt["payload"]["seq"], json!(1));

    // Same client_uuid again → identical ack, no second push.
    let retry = alice.cmd(send).await;
    assert_eq!(retry["ok"], json!(true), "retry send: {retry}");
    assert_eq!(retry["payload"]["seq"], first["payload"]["seq"]);
    assert_eq!(
        retry["payload"]["message_id"],
        first["payload"]["message_id"]
    );
    alice.expect_no_evt(SHORT).await;
}

/// First perf number for the project (roadmap Sprint 3 exit criterion): the
/// `channels.send` store-path p99. Unloaded sequential sends — a floor the
/// number can only rise from under load, so a floor well under 5 ms is the
/// meaningful check here. `#[ignore]` (a benchmark, not a gate); Sprint 4's
/// loadgen tracks the paced version, Sprint 10 tightens it.
/// Run: `cargo test --test channels_seq -- --ignored --nocapture send_latency_p99`.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
#[ignore]
async fn send_latency_p99(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let (_t, m) = mint_full(&app, tenant, world, "sender").await;
    let sender = m.identity.character_id;
    let channel = solo_channel(&app, world, sender).await;

    const N: usize = 2000;
    let mut micros: Vec<u128> = Vec::with_capacity(N);
    for _ in 0..N {
        let t = std::time::Instant::now();
        store::send_message(
            &app,
            world,
            sender,
            channel,
            new_id(),
            &json!({ "text": "m" }),
        )
        .await
        .expect("send");
        micros.push(t.elapsed().as_micros());
    }
    micros.sort_unstable();
    let p = |q: f64| micros[((N as f64 * q) as usize).min(N - 1)];
    println!(
        "channels.send store latency (n={N}): p50={}us p90={}us p99={}us max={}us",
        p(0.50),
        p(0.90),
        p(0.99),
        micros[N - 1],
    );
    // Generous ceiling: the send tx does ~4 round trips to a local Postgres.
    assert!(p(0.99) < 25_000, "p99 {}us over 25ms floor", p(0.99));
}

/// Concurrent `open_direct` of the same pair converges on one channel (the
/// ordered-pair unique index + ON CONFLICT path).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn concurrent_open_direct_one_channel(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 16).await;
    let (_ta, a) = mint_full(&app, tenant, world, "alice").await;
    let (_tb, b) = mint_full(&app, tenant, world, "bob").await;
    let caller = a.identity.character_id;
    let b_number = b.character.number.clone().expect("bob has a number");

    let mut handles = Vec::new();
    for _ in 0..8 {
        let app = app.clone();
        let num = b_number.clone();
        handles.push(tokio::spawn(async move {
            store::open_direct(&app, world, caller, &num).await
        }));
    }
    let mut ids = Vec::new();
    for h in handles {
        ids.push(h.await.expect("task").expect("open_direct"));
    }
    assert!(
        ids.windows(2).all(|w| w[0] == w[1]),
        "all concurrent opens must return the same channel: {ids:?}"
    );

    let dm_count: i64 = {
        let mut tx = world_tx(&app, world).await.expect("world_tx");
        sqlx::query_scalar("SELECT count(*) FROM channels WHERE kind = 'dm'")
            .fetch_one(&mut *tx)
            .await
            .expect("count dm channels")
    };
    assert_eq!(dm_count, 1, "exactly one pair channel");
}
