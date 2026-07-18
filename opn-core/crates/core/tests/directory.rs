//! Sprint 5 part B directory tests (OPN-CORE.md §10.7): contacts, blocks,
//! opaque resolve, block enforcement at `channels.open_direct`, and listings
//! with cursor paging + janitor expiry. RLS-on throughout (opn_app pool). No
//! MinIO needed — the one media touch (avatar ownership) is a DB count over a
//! directly-seeded `live` row.

mod common;

use common::{app_pool, seed_world_tenant, test_config, test_state};
use contracts::ErrCode;
use opn_core::infra::auth::Identity;
use opn_core::infra::db::world_tx;
use opn_core::primitives::{channels, directory, identity, Fail};
use opn_core::state::AppState;
use sqlx::PgPool;
use uuid::Uuid;

async fn state_and_alice(admin: &PgPool) -> (AppState, Identity, String) {
    let (world_id, tenant_id, _key) = seed_world_tenant(admin).await;
    let pool = app_pool(admin, 6).await;
    let state = test_state(pool, test_config()).await;
    let minted = identity::mint_session(&state.pg, tenant_id, world_id, "alice", None, 600)
        .await
        .expect("mint alice");
    let number = minted.character.number.clone().expect("number");
    (state, minted.identity, number)
}

async fn second(state: &AppState, first: &Identity, framework_ref: &str) -> (Identity, String) {
    let minted = identity::mint_session(
        &state.pg,
        first.tenant_id,
        first.world_id,
        framework_ref,
        None,
        600,
    )
    .await
    .expect("mint 2");
    (
        minted.identity,
        minted.character.number.clone().expect("number"),
    )
}

/// Seed a `live` media row owned by `who` without touching S3 (all_owned_live is
/// a DB count, so no object is needed).
async fn seed_live_media(state: &AppState, who: &Identity) -> Uuid {
    let id = Uuid::now_v7();
    let mut tx = world_tx(&state.pg, who.world_id).await.expect("tx");
    sqlx::query(
        "INSERT INTO media (id, world_id, owner_character, kind, mime, bytes, state) \
         VALUES ($1, $2, $3, 'photo', 'image/jpeg', 100, 'live')",
    )
    .bind(id)
    .bind(who.world_id)
    .bind(who.character_id)
    .execute(&mut *tx)
    .await
    .expect("seed media");
    tx.commit().await.expect("commit");
    id
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn contacts_crud_roundtrip(admin: PgPool) {
    let (state, alice, _num) = state_and_alice(&admin).await;

    // Create.
    directory::contact_upsert(&state, &alice, "555-0001", "Bob", None, None)
        .await
        .expect("create contact");
    let list = directory::contacts(&state, &alice).await.expect("list");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].number, "555-0001");
    assert_eq!(list[0].display_name, "Bob");

    // Update (upsert same number replaces the label, stays one row).
    directory::contact_upsert(&state, &alice, "555-0001", "Bobby", None, None)
        .await
        .expect("update contact");
    let list = directory::contacts(&state, &alice).await.expect("list");
    assert_eq!(list.len(), 1, "upsert replaces, not appends");
    assert_eq!(list[0].display_name, "Bobby");

    // Avatar ownership: a foreign/unknown media id is rejected.
    let bad = directory::contact_upsert(
        &state,
        &alice,
        "555-0002",
        "Carol",
        Some(Uuid::now_v7()),
        None,
    )
    .await;
    assert!(
        matches!(bad, Err(Fail::Code(ErrCode::Invalid))),
        "unowned avatar rejected",
    );

    // An owned live media is accepted as an avatar.
    let media = seed_live_media(&state, &alice).await;
    directory::contact_upsert(&state, &alice, "555-0002", "Carol", Some(media), None)
        .await
        .expect("owned avatar ok");

    // Empty inputs rejected.
    assert!(matches!(
        directory::contact_upsert(&state, &alice, "", "X", None, None).await,
        Err(Fail::Code(ErrCode::Invalid)),
    ));

    // Delete is idempotent.
    directory::contact_delete(&state, &alice, "555-0001")
        .await
        .expect("delete");
    directory::contact_delete(&state, &alice, "555-0001")
        .await
        .expect("delete again (no-op)");
    let list = directory::contacts(&state, &alice).await.expect("list");
    assert_eq!(list.len(), 1, "only Carol remains");
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn block_unblock_and_list(admin: PgPool) {
    let (state, alice, _num) = state_and_alice(&admin).await;

    directory::block(&state, &alice, "555-9999")
        .await
        .expect("block");
    // Idempotent: blocking twice keeps one row.
    directory::block(&state, &alice, "555-9999")
        .await
        .expect("block twice");
    let blocks = directory::blocks(&state, &alice).await.expect("blocks");
    assert_eq!(blocks, vec!["555-9999".to_string()]);

    directory::unblock(&state, &alice, "555-9999")
        .await
        .expect("unblock");
    directory::unblock(&state, &alice, "555-9999")
        .await
        .expect("unblock again (no-op)");
    let blocks = directory::blocks(&state, &alice).await.expect("blocks");
    assert!(blocks.is_empty());
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn resolve_unknown_known_and_blocked_indistinguishable(admin: PgPool) {
    let (state, alice, alice_num) = state_and_alice(&admin).await;
    let (bob, bob_num) = second(&state, &alice, "bob").await;

    // Unknown number → not reachable, no id ever on the wire.
    let unknown = directory::resolve_public(&state, &alice, "555-0000")
        .await
        .expect("resolve unknown");
    assert!(!unknown.reachable, "unknown is unreachable");
    assert!(unknown.display_name.is_none());

    // An over-long / empty number is capped before any indexed lookup runs.
    assert!(matches!(
        directory::resolve_public(&state, &alice, &"9".repeat(64)).await,
        Err(Fail::Code(ErrCode::Invalid)),
    ));
    assert!(matches!(
        directory::resolve_public(&state, &alice, "").await,
        Err(Fail::Code(ErrCode::Invalid)),
    ));

    // Known number → reachable; the caller's own saved label rides along.
    directory::contact_upsert(&state, &alice, &bob_num, "Bobby", None, None)
        .await
        .expect("save contact");
    let known = directory::resolve_public(&state, &alice, &bob_num)
        .await
        .expect("resolve known");
    assert!(known.reachable, "known number is reachable");
    assert_eq!(known.display_name.as_deref(), Some("Bobby"));

    // Callee-side block: Bob blocks Alice → Alice's resolve of Bob is now
    // reachable:false — byte-identical to the unknown-number result (privacy).
    directory::block(&state, &bob, &alice_num)
        .await
        .expect("bob blocks alice");
    let blocked = directory::resolve_public(&state, &alice, &bob_num)
        .await
        .expect("resolve blocked");
    assert!(!blocked.reachable, "a blocked pair is indistinguishable");

    // Caller-side block also makes it unreachable.
    directory::block(&state, &alice, &bob_num)
        .await
        .expect("alice blocks bob");
    let caller_blocked = directory::resolve_public(&state, &alice, &bob_num)
        .await
        .expect("resolve caller-blocked");
    assert!(!caller_blocked.reachable);
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn open_direct_blocked_pair_both_directions(admin: PgPool) {
    let (state, alice, alice_num) = state_and_alice(&admin).await;
    let (bob, bob_num) = second(&state, &alice, "bob").await;

    // Baseline: unblocked, the pair opens.
    assert!(
        channels::open_direct(&state, &alice, &bob_num)
            .await
            .is_ok(),
        "unblocked open works",
    );

    // Caller-side block: Alice blocks Bob's number. open_direct re-resolves
    // before the found-or-create, so even the channel that now exists doesn't
    // mask it → NotFound (indistinguishable from an unknown number).
    directory::block(&state, &alice, &bob_num)
        .await
        .expect("alice blocks bob");
    assert!(
        matches!(
            channels::open_direct(&state, &alice, &bob_num).await,
            Err(Fail::Code(ErrCode::NotFound)),
        ),
        "caller-side block → NotFound",
    );
    directory::unblock(&state, &alice, &bob_num)
        .await
        .expect("unblock");
    assert!(
        channels::open_direct(&state, &alice, &bob_num)
            .await
            .is_ok(),
        "unblock restores reachability",
    );

    // Callee-side block: Bob blocks Alice's number → Alice's open is NotFound
    // too. A block must bite in both directions and leak nothing.
    directory::block(&state, &bob, &alice_num)
        .await
        .expect("bob blocks alice");
    assert!(
        matches!(
            channels::open_direct(&state, &alice, &bob_num).await,
            Err(Fail::Code(ErrCode::NotFound)),
        ),
        "callee-side block → NotFound",
    );
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn listings_create_list_delete_expire(admin: PgPool) {
    let (state, alice, _num) = state_and_alice(&admin).await;
    let (bob, _bn) = second(&state, &alice, "bob").await;

    // Negative and absurdly-large TTL both rejected (the latter would otherwise
    // overflow now()+interval into a 500 instead of a clean Invalid).
    assert!(matches!(
        directory::listing_create(&state, &alice, "yp", "sale", "Bad", None, "555-1", Some(-1))
            .await,
        Err(Fail::Code(ErrCode::Invalid)),
    ));
    assert!(matches!(
        directory::listing_create(
            &state,
            &alice,
            "yp",
            "sale",
            "Bad",
            None,
            "555-1",
            Some(i64::MAX)
        )
        .await,
        Err(Fail::Code(ErrCode::Invalid)),
    ));

    // Post three under "yp".
    let mut ids = Vec::new();
    for i in 0..3 {
        let r = directory::listing_create(
            &state,
            &alice,
            "yp",
            "sale",
            &format!("Item {i}"),
            Some(serde_json::json!({ "price": i })),
            "555-1234",
            None,
        )
        .await
        .expect("create listing");
        ids.push(r["id"].as_str().expect("id").to_string());
    }

    // Cursor paging: limit 2 → page of 2 + a next cursor, then the last one.
    let page1 = directory::listings(&state, &alice, "yp", None, 2)
        .await
        .expect("page1");
    assert_eq!(page1.items.len(), 2);
    let cursor = page1.next_cursor.expect("has next");
    let cur = opn_core::infra::cursor::decode(&cursor).expect("decode cursor");
    let page2 = directory::listings(&state, &alice, "yp", Some(cur), 2)
        .await
        .expect("page2");
    assert_eq!(page2.items.len(), 1, "3 total across two pages");
    assert!(page2.next_cursor.is_none());
    // Newest-first, no dup/skip across the boundary.
    assert_eq!(page1.items[0].title, "Item 2");
    assert_eq!(page2.items[0].title, "Item 0");

    // Delete authz: Bob cannot delete Alice's listing (NotFound, no leak);
    // an unknown id is NotFound too.
    let victim: Uuid = ids[0].parse().expect("uuid");
    assert!(matches!(
        directory::listing_delete(&state, &bob, victim).await,
        Err(Fail::Code(ErrCode::NotFound)),
    ));
    assert!(matches!(
        directory::listing_delete(&state, &alice, Uuid::now_v7()).await,
        Err(Fail::Code(ErrCode::NotFound)),
    ));
    // Owner delete works.
    directory::listing_delete(&state, &alice, victim)
        .await
        .expect("owner delete");
    let after = directory::listings(&state, &alice, "yp", None, 10)
        .await
        .expect("list");
    assert_eq!(after.items.len(), 2, "one deleted");

    // Expiry: force one row's expires_at into the past, assert reads hide it and
    // the janitor deletes it.
    let survivor: Uuid = ids[1].parse().expect("uuid");
    {
        let mut tx = world_tx(&state.pg, alice.world_id).await.expect("tx");
        sqlx::query("UPDATE listings SET expires_at = now() - interval '1 hour' WHERE id = $1")
            .bind(survivor)
            .execute(&mut *tx)
            .await
            .expect("expire");
        tx.commit().await.expect("commit");
    }
    let visible = directory::listings(&state, &alice, "yp", None, 10)
        .await
        .expect("list");
    // Exactly the one still-active row remains visible (the expired one hidden,
    // the deleted one gone) — an `.all()` alone would pass vacuously.
    let survivor_str = ids[1].clone();
    let active_str = ids[2].clone();
    assert_eq!(visible.items.len(), 1, "only the active listing is visible");
    assert_eq!(
        visible.items[0].id.to_string(),
        active_str,
        "the visible row is the never-expiring one",
    );
    assert!(
        visible
            .items
            .iter()
            .all(|l| l.id.to_string() != survivor_str),
        "expired row hidden at read time",
    );
    let deleted = opn_core::janitor::listings_expire(&state.pg)
        .await
        .expect("janitor");
    assert_eq!(deleted, 1, "janitor deletes exactly the expired row");
}

/// Cross-world RLS canary for every Sprint 5 table (roadmap Sprint 1 exit
/// pattern, lapsed since media part A; Sprint 9 generates the exhaustive
/// version). Load-bearing for `blocks` especially: resolve's two subqueries
/// carry no `world_id` predicate — RLS is the *only* thing scoping them. The
/// probe is a raw unfiltered `SELECT count(*)` under the other world's tx, so a
/// zero can only come from the policy, and the same count under the owning
/// world proves the rows exist (no vacuous pass).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn cross_world_rls_isolation(admin: PgPool) {
    let (state, alice, _num) = state_and_alice(&admin).await;
    // A second, unrelated world in the same database.
    let (world_b, _tenant_b, _key_b) = seed_world_tenant(&admin).await;

    // One row per table, all owned by world-A's alice.
    directory::contact_upsert(&state, &alice, "555-0001", "Bob", None, None)
        .await
        .expect("contact");
    directory::block(&state, &alice, "555-9999")
        .await
        .expect("block");
    directory::listing_create(&state, &alice, "yp", "sale", "Item", None, "555-1", None)
        .await
        .expect("listing");
    seed_live_media(&state, &alice).await;

    // Literal statements — sqlx wants 'static SQL (see channels/store.rs note).
    let probes: [(&str, &str); 4] = [
        ("contacts", "SELECT count(*) FROM contacts"),
        ("blocks", "SELECT count(*) FROM blocks"),
        ("listings", "SELECT count(*) FROM listings"),
        ("media", "SELECT count(*) FROM media"),
    ];
    for (table, count_sql) in probes {
        let mut tx_a = world_tx(&state.pg, alice.world_id).await.expect("tx a");
        let in_a: i64 = sqlx::query_scalar(count_sql)
            .fetch_one(&mut *tx_a)
            .await
            .expect("count in world a");
        tx_a.commit().await.expect("commit");
        assert_eq!(in_a, 1, "{table}: owning world sees its row");

        let mut tx_b = world_tx(&state.pg, world_b).await.expect("tx b");
        let in_b: i64 = sqlx::query_scalar(count_sql)
            .fetch_one(&mut *tx_b)
            .await
            .expect("count in world b");
        tx_b.commit().await.expect("commit");
        assert_eq!(in_b, 0, "{table}: cross-world read must be empty (RLS)");
    }
}
