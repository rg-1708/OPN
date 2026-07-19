//! Sprint 7 part B exchange tests (OPN-CORE.md §10.5 item 4, OPN.md §14.2): the
//! framework exchange — deposit, the two-leg withdraw, the reconciliation
//! cross-check, and the bridge journal. Builds on part A's ledger. HTTP paths
//! (deposit / withdraw_confirm / journal) hit the real `app_router` with an API
//! key (rule 3); the WS `ledger.withdraw` leg is driven over a real socket; money
//! assertions read the DB directly (sharper than draining events, à la ledger.rs).

mod common;

use std::net::SocketAddr;

use common::ws::{connect_and_auth, mint_token, spawn_server};
use common::{app_pool, seed_world_tenant, test_config, test_state};
use opn_core::infra::db::world_tx;
use opn_core::primitives::ledger;
use opn_core::state::AppState;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

// ── HTTP + DB helpers ─────────────────────────────────────────────────────────

/// POST the exchange endpoint with an API key; returns `(status, json)`.
async fn post_exchange(addr: SocketAddr, key: &str, body: Value) -> (u16, Value) {
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/tenants/self/exchange"))
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .expect("POST /exchange");
    let status = resp.status().as_u16();
    let text = resp.text().await.expect("body");
    let v = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text }));
    (status, v)
}

async fn get_journal(addr: SocketAddr, key: &str, since: Option<&str>) -> Value {
    let mut url = format!("http://{addr}/v1/tenants/self/exchange");
    if let Some(s) = since {
        url.push_str(&format!("?since={}", urlencode(s)));
    }
    let resp = reqwest::Client::new()
        .get(url)
        .header("authorization", format!("Bearer {key}"))
        .send()
        .await
        .expect("GET /exchange");
    assert_eq!(resp.status().as_u16(), 200, "journal status");
    let text = resp.text().await.expect("body");
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("journal json ({e}): {text}"))
}

/// Minimal RFC-3339 percent-encoding (`:` and `+`) — enough for a query value.
fn urlencode(s: &str) -> String {
    s.replace('+', "%2B").replace(':', "%3A")
}

async fn wallet_of(state: &AppState, world: Uuid, character: Uuid) -> Option<Uuid> {
    let mut tx = world_tx(&state.pg, world).await.expect("tx");
    let id: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM accounts WHERE owner_character = $1 AND owner_kind = 'character'",
    )
    .bind(character)
    .fetch_optional(&mut *tx)
    .await
    .expect("wallet lookup");
    tx.commit().await.expect("commit");
    id
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

async fn exchange_state(state: &AppState, world: Uuid, id: &str) -> String {
    let mut tx = world_tx(&state.pg, world).await.expect("tx");
    let s: String =
        sqlx::query_scalar("SELECT state FROM exchanges WHERE world_id = $1 AND id = $2")
            .bind(world)
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .expect("exchange state");
    tx.commit().await.expect("commit");
    s
}

async fn count(state: &AppState, world: Uuid, sql: &'static str) -> i64 {
    let mut tx = world_tx(&state.pg, world).await.expect("tx");
    let n: i64 = sqlx::query_scalar(sql)
        .fetch_one(&mut *tx)
        .await
        .expect("count");
    tx.commit().await.expect("commit");
    n
}

// ═══════════════════════════════════════════════════════════════════════════
// deposit
// ═══════════════════════════════════════════════════════════════════════════

/// Deposit auto-creates the wallet, credits it, and is idempotent on the
/// bridge-chosen id: the same `exchange_id` five times credits exactly once and
/// writes exactly one `deposit` transfer + one exchange row (§10.5 item 4).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn deposit_creates_wallet_and_is_idempotent(admin: PgPool) {
    let (world, tenant, key) = seed_world_tenant(&admin).await;
    let pool = app_pool(&admin, 8).await;
    let state = test_state(pool, test_config()).await;
    let (_t, alice) = mint_token(&state.pg, tenant, world, "alice").await;
    let server = spawn_server(state.clone()).await;

    assert!(
        wallet_of(&state, world, alice.character_id).await.is_none(),
        "no wallet before first deposit"
    );

    let body = json!({
        "exchange_id": "dep-1",
        "character_id": alice.character_id,
        "amount": 500,
        "direction": "deposit",
    });
    for i in 0..5 {
        let (status, v) = post_exchange(server.addr, &key, body.clone()).await;
        assert_eq!(status, 200, "deposit {i} status: {v}");
        assert_eq!(v["state"], json!("done"), "deposit {i} done: {v}");
        assert_eq!(v["amount"], json!(500));
    }

    let wallet = wallet_of(&state, world, alice.character_id)
        .await
        .expect("wallet created by deposit");
    assert_eq!(
        balance(&state, world, wallet).await,
        500,
        "credited exactly once despite 5 identical deposits"
    );
    assert_eq!(
        count(&state, world, "SELECT count(*) FROM exchanges").await,
        1,
        "one exchange row"
    );
    assert_eq!(
        count(
            &state,
            world,
            "SELECT count(*) FROM transfers WHERE kind = 'deposit'"
        )
        .await,
        1,
        "one deposit transfer leg"
    );
    // The system account went negative funding the wallet (it is the mint).
    let system: i64 = count(
        &state,
        world,
        "SELECT balance FROM accounts WHERE owner_kind = 'system'",
    )
    .await;
    assert_eq!(system, -500, "system minted 500");

    // A deposit to a nonexistent character → not_found (404).
    let (status, _v) = post_exchange(
        server.addr,
        &key,
        json!({ "exchange_id": "dep-x", "character_id": Uuid::now_v7(), "amount": 10, "direction": "deposit" }),
    )
    .await;
    assert_eq!(status, 404, "deposit to unknown character");
}

// ═══════════════════════════════════════════════════════════════════════════
// withdraw (two legs)
// ═══════════════════════════════════════════════════════════════════════════

/// Full withdraw cycle (§10.5 item 4): WS `ledger.withdraw` holds the wallet +
/// opens a pending exchange; the reservation is excluded from available; the
/// bridge's `withdraw_confirm` captures the hold to `system` (`done`); a
/// re-confirm is idempotent; reconciliation's cross-check freezes nothing.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn withdraw_full_cycle(admin: PgPool) {
    let (world, tenant, key) = seed_world_tenant(&admin).await;
    let pool = app_pool(&admin, 8).await;
    let state = test_state(pool, test_config()).await;
    let (token, alice) = mint_token(&state.pg, tenant, world, "alice").await;
    let server = spawn_server(state.clone()).await;

    // Fund the wallet with a deposit of 1000.
    let (status, _) = post_exchange(
        server.addr,
        &key,
        json!({ "exchange_id": "fund", "character_id": alice.character_id, "amount": 1000, "direction": "deposit" }),
    )
    .await;
    assert_eq!(status, 200, "fund deposit");
    let wallet = wallet_of(&state, world, alice.character_id)
        .await
        .expect("wallet");

    // WS leg 1: withdraw 400 → hold + pending exchange.
    let mut c = connect_and_auth(server.addr, &token).await;
    let ack = c
        .cmd(json!({ "cmd": "ledger.withdraw", "payload": { "amount": 400 } }))
        .await;
    assert_eq!(ack["ok"], json!(true), "withdraw ack: {ack}");
    let exchange_id = ack["payload"]["exchange_id"]
        .as_str()
        .expect("exchange_id")
        .to_string();
    assert_eq!(
        exchange_state(&state, world, &exchange_id).await,
        "pending_confirm"
    );
    // Balance unmoved yet (the 400 is only reserved).
    assert_eq!(
        balance(&state, world, wallet).await,
        1000,
        "hold reserves, not moves"
    );

    // The 400 hold is excluded from available: a second withdraw of 700 → conflict.
    let ack2 = c
        .cmd(json!({ "cmd": "ledger.withdraw", "payload": { "amount": 700 } }))
        .await;
    assert_eq!(ack2["ok"], json!(false), "over-available withdraw refused");
    assert_eq!(ack2["err"]["code"], json!("conflict"), "{ack2}");

    // Leg 2: the bridge confirms → capture the hold to system.
    let (status, v) = post_exchange(
        server.addr,
        &key,
        json!({ "exchange_id": exchange_id, "character_id": alice.character_id, "amount": 400, "direction": "withdraw_confirm" }),
    )
    .await;
    assert_eq!(status, 200, "confirm: {v}");
    assert_eq!(v["state"], json!("done"));
    assert_eq!(exchange_state(&state, world, &exchange_id).await, "done");
    assert_eq!(
        balance(&state, world, wallet).await,
        600,
        "confirmed withdraw debits the wallet"
    );

    // Idempotent re-confirm: still done, no double capture.
    let (status, _) = post_exchange(
        server.addr,
        &key,
        json!({ "exchange_id": exchange_id, "character_id": alice.character_id, "amount": 400, "direction": "withdraw_confirm" }),
    )
    .await;
    assert_eq!(status, 200, "idempotent re-confirm");
    assert_eq!(balance(&state, world, wallet).await, 600, "no double debit");

    // The settle wrote a `kind='withdraw'` transfer leg (what the cross-check
    // keys on — asserted explicitly, not just implied by reconcile-clean).
    assert_eq!(
        count(
            &state,
            world,
            "SELECT count(*) FROM transfers WHERE kind = 'withdraw'"
        )
        .await,
        1,
        "one withdraw transfer leg"
    );
    // Reconciliation cross-check is satisfied (dep 1000 == leg; wdr 400 == leg).
    assert!(
        ledger::store::reconcile(&state.pg, world)
            .await
            .expect("reconcile")
            .is_empty(),
        "a consistent exchange ledger freezes nothing"
    );

    // A confirm of a deposit id ('fund') → not_found (the direction filter), a
    // distinct path from the field-mismatch → invalid case (see
    // withdraw_confirm_rejects_mismatch).
    let (status, _) = post_exchange(
        server.addr,
        &key,
        json!({ "exchange_id": "fund", "character_id": alice.character_id, "amount": 1, "direction": "withdraw_confirm" }),
    )
    .await;
    assert_eq!(status, 404, "confirm of a non-withdraw id");
}

/// Unconfirmed withdraw (§10.5 item 4): the hold expires, the janitor releases it
/// AND flips the exchange to `expired`, the reservation is freed, and a late
/// confirm is refused.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn withdraw_expiry(admin: PgPool) {
    let (world, tenant, key) = seed_world_tenant(&admin).await;
    let pool = app_pool(&admin, 8).await;
    let state = test_state(pool, test_config()).await;
    let (token, alice) = mint_token(&state.pg, tenant, world, "alice").await;
    let server = spawn_server(state.clone()).await;

    post_exchange(
        server.addr,
        &key,
        json!({ "exchange_id": "fund", "character_id": alice.character_id, "amount": 1000, "direction": "deposit" }),
    )
    .await;
    let wallet = wallet_of(&state, world, alice.character_id)
        .await
        .expect("wallet");

    let mut c = connect_and_auth(server.addr, &token).await;
    let ack = c
        .cmd(json!({ "cmd": "ledger.withdraw", "payload": { "amount": 300 } }))
        .await;
    let exchange_id = ack["payload"]["exchange_id"]
        .as_str()
        .expect("id")
        .to_string();

    // Age the backing hold past its expiry.
    {
        let mut tx = world_tx(&state.pg, world).await.expect("tx");
        sqlx::query(
            "UPDATE holds SET expires_at = now() - interval '1 minute' \
             WHERE account_id = $1 AND state = 'held'",
        )
        .bind(wallet)
        .execute(&mut *tx)
        .await
        .expect("age hold");
        tx.commit().await.expect("commit");
    }

    let released = ledger::store::expire_holds(&state.pg, world)
        .await
        .expect("expire");
    assert_eq!(released, vec![(alice.character_id, 300)], "hold released");
    assert_eq!(
        exchange_state(&state, world, &exchange_id).await,
        "expired",
        "the exchange auto-expires with its hold"
    );

    // Reserve freed: available is the full 1000 again — another withdraw of 1000 ok.
    let ack2 = c
        .cmd(json!({ "cmd": "ledger.withdraw", "payload": { "amount": 1000 } }))
        .await;
    assert_eq!(ack2["ok"], json!(true), "reservation freed: {ack2}");

    // A late confirm on the expired exchange → conflict.
    let (status, _) = post_exchange(
        server.addr,
        &key,
        json!({ "exchange_id": exchange_id, "character_id": alice.character_id, "amount": 300, "direction": "withdraw_confirm" }),
    )
    .await;
    assert_eq!(status, 409, "confirm of an expired withdraw");
}

// ═══════════════════════════════════════════════════════════════════════════
// journal + reconcile cross-check + RLS
// ═══════════════════════════════════════════════════════════════════════════

/// The bridge journal (`GET .../exchange?since`) lists the world's exchanges
/// oldest-first with the right direction/state/amount, and `since` filters.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn journal_lists_and_filters(admin: PgPool) {
    let (world, tenant, key) = seed_world_tenant(&admin).await;
    let pool = app_pool(&admin, 8).await;
    let state = test_state(pool, test_config()).await;
    let (_t, alice) = mint_token(&state.pg, tenant, world, "alice").await;
    let (_t2, bob) = mint_token(&state.pg, tenant, world, "bob").await;
    let server = spawn_server(state.clone()).await;

    post_exchange(
        server.addr,
        &key,
        json!({ "exchange_id": "d1", "character_id": alice.character_id, "amount": 100, "direction": "deposit" }),
    )
    .await;
    post_exchange(
        server.addr,
        &key,
        json!({ "exchange_id": "d2", "character_id": bob.character_id, "amount": 200, "direction": "deposit" }),
    )
    .await;

    let all = get_journal(server.addr, &key, None).await;
    let items = all["items"].as_array().expect("items array");
    assert_eq!(items.len(), 2, "both exchanges: {all}");
    assert_eq!(items[0]["id"], json!("d1"), "oldest first");
    assert_eq!(items[0]["direction"], json!("deposit"));
    assert_eq!(items[0]["state"], json!("done"));
    assert_eq!(items[0]["amount"], json!(100));
    assert_eq!(items[1]["id"], json!("d2"));

    // `since` at d2's created_at drops d1 (inclusive keyset; d1 is strictly older).
    let d2_at = items[1]["created_at"].as_str().expect("created_at");
    let filtered = get_journal(server.addr, &key, Some(d2_at)).await;
    let fitems = filtered["items"].as_array().expect("items");
    assert!(
        fitems.iter().all(|i| i["id"] != json!("d1")),
        "since excludes the older d1: {filtered}"
    );
    assert!(
        fitems.iter().any(|i| i["id"] == json!("d2")),
        "since is inclusive of d2"
    );
}

/// Reconciliation cross-check (§10.5 item 7): an exchange row without its money
/// leg (Σ exchanges ≠ Σ transfer legs) freezes the system account, halting
/// further exchange flow. The per-account balance recompute alone can't see this.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn reconcile_cross_check_freezes_orphan_exchange(admin: PgPool) {
    let (world, tenant, key) = seed_world_tenant(&admin).await;
    let pool = app_pool(&admin, 8).await;
    let state = test_state(pool, test_config()).await;
    let (_t, alice) = mint_token(&state.pg, tenant, world, "alice").await;
    let server = spawn_server(state.clone()).await;

    post_exchange(
        server.addr,
        &key,
        json!({ "exchange_id": "d1", "character_id": alice.character_id, "amount": 500, "direction": "deposit" }),
    )
    .await;
    assert!(
        ledger::store::reconcile(&state.pg, world)
            .await
            .expect("reconcile")
            .is_empty(),
        "a consistent exchange ledger is clean"
    );

    // Inject an orphan exchange (a done deposit with no matching transfer leg).
    {
        let mut tx = world_tx(&state.pg, world).await.expect("tx");
        sqlx::query(
            "INSERT INTO exchanges (world_id, id, character_id, amount, direction, state) \
             VALUES ($1, 'orphan', $2, 999, 'deposit', 'done')",
        )
        .bind(world)
        .bind(alice.character_id)
        .execute(&mut *tx)
        .await
        .expect("inject orphan");
        tx.commit().await.expect("commit");
    }

    let frozen = ledger::store::reconcile(&state.pg, world)
        .await
        .expect("reconcile");
    assert_eq!(frozen.len(), 1, "the system account is frozen: {frozen:?}");

    // Exchange flow now halts: a fresh deposit hits the frozen system → conflict.
    let (status, _) = post_exchange(
        server.addr,
        &key,
        json!({ "exchange_id": "d2", "character_id": alice.character_id, "amount": 10, "direction": "deposit" }),
    )
    .await;
    assert_eq!(status, 409, "deposit onto a frozen system is refused");
}

/// A second deposit (different id) to an already-existing wallet credits
/// ADDITIVELY (§10.5 item 4): the wallet the first deposit created is reused and
/// summed, not overwritten. (The single-deposit happy path can't see an
/// overwrite-instead-of-add bug.)
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn second_deposit_credits_additively(admin: PgPool) {
    let (world, tenant, key) = seed_world_tenant(&admin).await;
    let pool = app_pool(&admin, 8).await;
    let state = test_state(pool, test_config()).await;
    let (_t, alice) = mint_token(&state.pg, tenant, world, "alice").await;
    let server = spawn_server(state.clone()).await;

    for (id, amt) in [("d1", 500), ("d2", 300)] {
        let (status, _) = post_exchange(
            server.addr,
            &key,
            json!({ "exchange_id": id, "character_id": alice.character_id, "amount": amt, "direction": "deposit" }),
        )
        .await;
        assert_eq!(status, 200, "deposit {id}");
    }
    let wallet = wallet_of(&state, world, alice.character_id)
        .await
        .expect("wallet");
    assert_eq!(
        balance(&state, world, wallet).await,
        800,
        "500 + 300 additive"
    );
    assert_eq!(
        count(&state, world, "SELECT count(*) FROM exchanges").await,
        2,
        "two exchange rows"
    );
    assert_eq!(
        count(
            &state,
            world,
            "SELECT count(*) FROM transfers WHERE kind = 'deposit'"
        )
        .await,
        2,
        "two deposit legs"
    );
}

/// A fresh deposit notifies the credited character exactly once (incoming money,
/// §10.5 item 8); an idempotent replay does NOT re-notify (the `fresh`/`credited`
/// gating). Alice has no live socket, so the notify lands in her inbox.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn deposit_notifies_once(admin: PgPool) {
    let (world, tenant, key) = seed_world_tenant(&admin).await;
    let pool = app_pool(&admin, 8).await;
    let state = test_state(pool, test_config()).await;
    let (_t, alice) = mint_token(&state.pg, tenant, world, "alice").await;
    let server = spawn_server(state.clone()).await;

    let body = json!({ "exchange_id": "d1", "character_id": alice.character_id, "amount": 500, "direction": "deposit" });
    for _ in 0..3 {
        post_exchange(server.addr, &key, body.clone()).await;
    }
    let n = count(
        &state,
        world,
        "SELECT count(*) FROM inbox WHERE kind = 'transfer_in'",
    )
    .await;
    assert_eq!(n, 1, "fresh deposit notifies once; replays don't re-notify");
}

/// `withdraw_confirm` rejects a request whose `character_id` or `amount` disagrees
/// with the stored pending exchange (§10.5 item 4) — a bridge validation. Neither
/// mismatch settles the hold; a correct confirm still works after.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn withdraw_confirm_rejects_mismatch(admin: PgPool) {
    let (world, tenant, key) = seed_world_tenant(&admin).await;
    let pool = app_pool(&admin, 8).await;
    let state = test_state(pool, test_config()).await;
    let (token, alice) = mint_token(&state.pg, tenant, world, "alice").await;
    let (_t2, bob) = mint_token(&state.pg, tenant, world, "bob").await;
    let server = spawn_server(state.clone()).await;

    post_exchange(
        server.addr,
        &key,
        json!({ "exchange_id": "fund", "character_id": alice.character_id, "amount": 1000, "direction": "deposit" }),
    )
    .await;
    let mut c = connect_and_auth(server.addr, &token).await;
    let ack = c
        .cmd(json!({ "cmd": "ledger.withdraw", "payload": { "amount": 400 } }))
        .await;
    let xid = ack["payload"]["exchange_id"]
        .as_str()
        .expect("id")
        .to_string();

    // Wrong character → invalid; wrong amount → invalid. Neither settles.
    let (status, _) = post_exchange(
        server.addr,
        &key,
        json!({ "exchange_id": xid, "character_id": bob.character_id, "amount": 400, "direction": "withdraw_confirm" }),
    )
    .await;
    assert_eq!(status, 400, "wrong character_id → invalid");
    let (status, _) = post_exchange(
        server.addr,
        &key,
        json!({ "exchange_id": xid, "character_id": alice.character_id, "amount": 399, "direction": "withdraw_confirm" }),
    )
    .await;
    assert_eq!(status, 400, "wrong amount → invalid");
    assert_eq!(
        exchange_state(&state, world, &xid).await,
        "pending_confirm",
        "a rejected confirm leaves the exchange pending"
    );

    // The correct confirm still works.
    let (status, _) = post_exchange(
        server.addr,
        &key,
        json!({ "exchange_id": xid, "character_id": alice.character_id, "amount": 400, "direction": "withdraw_confirm" }),
    )
    .await;
    assert_eq!(status, 200, "correct confirm settles");
    assert_eq!(exchange_state(&state, world, &xid).await, "done");
}

/// `ledger.withdraw` for a character who has never received a deposit (no wallet)
/// → conflict, not an opaque internal (§10.5 item 4).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn withdraw_without_wallet_conflicts(admin: PgPool) {
    let (world, tenant, _key) = seed_world_tenant(&admin).await;
    let pool = app_pool(&admin, 8).await;
    let state = test_state(pool, test_config()).await;
    let (token, _alice) = mint_token(&state.pg, tenant, world, "alice").await;
    let server = spawn_server(state.clone()).await;

    let mut c = connect_and_auth(server.addr, &token).await;
    let ack = c
        .cmd(json!({ "cmd": "ledger.withdraw", "payload": { "amount": 100 } }))
        .await;
    assert_eq!(ack["ok"], json!(false), "no wallet → refused: {ack}");
    assert_eq!(ack["err"]["code"], json!("conflict"), "{ack}");
}

/// The cross-check's WITHDRAW half (§10.5 item 7): an orphan `withdraw`/`done`
/// exchange with no matching `kind='withdraw'` transfer freezes the system —
/// the mirror of the deposit-orphan case, so the withdraw half of the detector
/// can't be silently dead. (Both halves share the same freeze machinery.)
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn cross_check_freezes_orphan_withdraw(admin: PgPool) {
    let (world, tenant, key) = seed_world_tenant(&admin).await;
    let pool = app_pool(&admin, 8).await;
    let state = test_state(pool, test_config()).await;
    let (_t, alice) = mint_token(&state.pg, tenant, world, "alice").await;
    let server = spawn_server(state.clone()).await;
    // A real deposit so a system account exists to freeze.
    post_exchange(
        server.addr,
        &key,
        json!({ "exchange_id": "d1", "character_id": alice.character_id, "amount": 500, "direction": "deposit" }),
    )
    .await;

    {
        let mut tx = world_tx(&state.pg, world).await.expect("tx");
        sqlx::query(
            "INSERT INTO exchanges (world_id, id, character_id, amount, direction, state) \
             VALUES ($1, 'w-orphan', $2, 250, 'withdraw', 'done')",
        )
        .bind(world)
        .bind(alice.character_id)
        .execute(&mut *tx)
        .await
        .expect("inject orphan withdraw");
        tx.commit().await.expect("commit");
    }
    let frozen = ledger::store::reconcile(&state.pg, world)
        .await
        .expect("reconcile");
    assert_eq!(
        frozen.len(),
        1,
        "withdraw-side drift freezes system: {frozen:?}"
    );
}

/// Concurrent deposits with the SAME `exchange_id` credit exactly once — the
/// `exchanges` PK is the concurrent backstop (a racing duplicate rolls back and
/// the retry hits the idempotent path). Money can't be double-created.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn concurrent_same_id_deposit_credits_once(admin: PgPool) {
    let (world, tenant, key) = seed_world_tenant(&admin).await;
    let pool = app_pool(&admin, 16).await;
    let state = test_state(pool, test_config()).await;
    let (_t, alice) = mint_token(&state.pg, tenant, world, "alice").await;
    let server = spawn_server(state.clone()).await;

    let body = json!({ "exchange_id": "dup", "character_id": alice.character_id, "amount": 500, "direction": "deposit" });
    let mut handles = Vec::new();
    for _ in 0..8 {
        let (addr, key, body) = (server.addr, key.clone(), body.clone());
        handles.push(tokio::spawn(async move {
            post_exchange(addr, &key, body).await.0
        }));
    }
    // At least one succeeds; a racing duplicate may 500 (rolled back) then a
    // retry would succeed — either way, no double credit.
    for h in handles {
        let _ = h.await.expect("join");
    }
    // A final sequential deposit with the same id must find it already done.
    let (status, v) = post_exchange(server.addr, &key, body).await;
    assert_eq!(status, 200, "idempotent settle: {v}");

    let wallet = wallet_of(&state, world, alice.character_id)
        .await
        .expect("wallet");
    assert_eq!(
        balance(&state, world, wallet).await,
        500,
        "credited exactly once"
    );
    assert_eq!(
        count(
            &state,
            world,
            "SELECT count(*) FROM exchanges WHERE id = 'dup'"
        )
        .await,
        1,
        "one exchange row despite the race"
    );
}

/// A withdraw's backing hold is NOT reachable by the public `ledger.capture` /
/// `ledger.release` API (§10.5 item 4): it reads as a nonexistent hold, so a
/// character can't settle their own withdraw reservation elsewhere and leave the
/// exchange pending for the bridge to confirm → money created (adversarial
/// review, Sprint 7B). Verified at the store seam (the hold_id is never on the
/// wire, so this is the reachable-only-by-guess path made explicit).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn withdraw_hold_not_capturable_by_user(admin: PgPool) {
    use contracts::ErrCode;
    use opn_core::primitives::Fail;
    let (world, tenant, key) = seed_world_tenant(&admin).await;
    let pool = app_pool(&admin, 8).await;
    let state = test_state(pool, test_config()).await;
    let (token, alice) = mint_token(&state.pg, tenant, world, "alice").await;
    let (_t2, bob) = mint_token(&state.pg, tenant, world, "bob").await;
    let server = spawn_server(state.clone()).await;
    post_exchange(
        server.addr,
        &key,
        json!({ "exchange_id": "fund", "character_id": alice.character_id, "amount": 1000, "direction": "deposit" }),
    )
    .await;
    let mut c = connect_and_auth(server.addr, &token).await;
    c.cmd(json!({ "cmd": "ledger.withdraw", "payload": { "amount": 400 } }))
        .await;

    // The withdraw hold id (found out-of-band here; never on the wire in prod).
    let hold_id: Uuid = {
        let mut tx = world_tx(&state.pg, world).await.expect("tx");
        let id = sqlx::query_scalar("SELECT id FROM holds WHERE state = 'held'")
            .fetch_one(&mut *tx)
            .await
            .expect("hold id");
        tx.commit().await.expect("commit");
        id
    };
    let bob_wallet = {
        // Give bob a wallet so a capture destination exists.
        post_exchange(
            server.addr,
            &key,
            json!({ "exchange_id": "fund-b", "character_id": bob.character_id, "amount": 1, "direction": "deposit" }),
        )
        .await;
        wallet_of(&state, world, bob.character_id)
            .await
            .expect("bob wallet")
    };

    // Capture and release both see it as nonexistent → NotFound (the guard
    // filters an exchange-backing pending hold out of the lookup).
    assert!(
        matches!(
            ledger::capture(&state, &alice, hold_id, bob_wallet).await,
            Err(Fail::Code(ErrCode::NotFound))
        ),
        "capture of a withdraw hold is refused as not-found"
    );
    assert!(
        matches!(
            ledger::release(&state, &alice, hold_id).await,
            Err(Fail::Code(ErrCode::NotFound))
        ),
        "release of a withdraw hold is refused as not-found"
    );
    // The exchange is still pending and the funds still reserved (no leak).
    let mut tx = world_tx(&state.pg, world).await.expect("tx");
    let held: i64 = sqlx::query_scalar("SELECT count(*) FROM holds WHERE state = 'held'")
        .fetch_one(&mut *tx)
        .await
        .expect("held count");
    tx.commit().await.expect("commit");
    assert_eq!(
        held, 1,
        "the reservation is intact after the refused attempts"
    );
}

/// Cross-world RLS isolation for `exchanges` (mirrors ledger.rs): an exchange in
/// world A is invisible under world B's tx.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn cross_world_rls_isolation(admin: PgPool) {
    let (world, tenant, key) = seed_world_tenant(&admin).await;
    let pool = app_pool(&admin, 8).await;
    let state = test_state(pool, test_config()).await;
    let (_t, alice) = mint_token(&state.pg, tenant, world, "alice").await;
    let server = spawn_server(state.clone()).await;
    post_exchange(
        server.addr,
        &key,
        json!({ "exchange_id": "d1", "character_id": alice.character_id, "amount": 100, "direction": "deposit" }),
    )
    .await;
    let (world_b, _tenant_b, _key_b) = seed_world_tenant(&admin).await;

    let in_a = count(&state, world, "SELECT count(*) FROM exchanges").await;
    assert!(in_a > 0, "owning world sees its exchange");
    let in_b = count(&state, world_b, "SELECT count(*) FROM exchanges").await;
    assert_eq!(in_b, 0, "cross-world exchange read is empty (RLS)");
}
