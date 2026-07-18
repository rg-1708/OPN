//! Cross-replica Redis fan-out (roadmap Sprint 2 item 10, OPN-CORE.md §3/§8):
//! two in-process gateway "replicas" sharing one Redis. Proves a `publish` on
//! replica A reaches a subscriber on replica B, and that A's own subscriber
//! receives the event exactly once (self-drop, not double-delivered).
//!
//! Needs the dev stack up (Postgres + Redis on localhost).

mod common;

use std::time::Duration;

use common::{app_pool, seed_world_tenant};
use contracts::Evt;
use opn_core::gateway::registry::ConnHandle;
use opn_core::state::AppState;
use serde_json::Value;
use sqlx::PgPool;

/// A gateway "replica": its own state + registry, `replicas = 2` so
/// `gateway::publish` takes the remote (Redis) path, sharing one Redis.
async fn replica(app: &PgPool) -> AppState {
    let mut cfg = common::test_config();
    cfg.replicas = 2;
    common::test_state(app.clone(), cfg).await
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn cross_replica_fanout_and_self_drop(admin: PgPool) {
    let (world_id, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 4).await;

    // Two replicas sharing one Redis. Both listen for remote events.
    let state_a = replica(&app).await;
    let state_b = replica(&app).await;
    let _la = opn_core::gateway::fanout::spawn_listener(state_a.clone());
    let _lb = opn_core::gateway::fanout::spawn_listener(state_b.clone());

    // Two real minted sessions → two Identities → two live handles. Registered
    // on different registries (A and B), so the same session id would be fine,
    // but distinct sessions keep the topic/character wiring unambiguous.
    let minted_a =
        opn_core::primitives::identity::mint_session(&app, tenant, world_id, "ca", None, 600)
            .await
            .expect("mint session a");
    let minted_b =
        opn_core::primitives::identity::mint_session(&app, tenant, world_id, "cb", None, 600)
            .await
            .expect("mint session b");

    let character_id = minted_b.identity.character_id;
    let topic = format!("presence:{character_id}");
    let evt = Evt::PresenceState {
        character_id,
        online: Some(true),
        last_seen_at: None,
    };

    // Subscriber on replica B.
    let (handle_b, mut rx_b, _closed_b) = ConnHandle::new(minted_b.identity, true, 256);
    state_b.registry.register(handle_b.clone());
    state_b.registry.subscribe(&topic, &handle_b);

    // Subscriber on replica A (same topic) — must receive exactly once.
    let (handle_a, mut rx_a, _closed_a) = ConnHandle::new(minted_a.identity, true, 256);
    state_a.registry.register(handle_a.clone());
    state_a.registry.subscribe(&topic, &handle_a);

    // Give both listeners a moment to psubscribe before the first publish.
    tokio::time::sleep(Duration::from_millis(300)).await;

    opn_core::gateway::publish(&state_a, world_id, &topic, &evt).await;

    // B receives it over Redis.
    let frame_b = tokio::time::timeout(Duration::from_secs(5), rx_b.recv())
        .await
        .expect("B: no frame within 5s (fan-out did not cross replicas)")
        .expect("B: channel closed");
    let v: Value = serde_json::from_str(&frame_b).expect("B: frame is json");
    assert_eq!(
        v["topic"],
        Value::String(topic.clone()),
        "B: topic mismatch"
    );
    assert_eq!(v["evt"], "presence.state", "B: evt mismatch");
    assert_eq!(
        v["payload"]["character_id"],
        Value::String(character_id.to_string()),
        "B: character mismatch"
    );

    // A receives the local publish once...
    let frame_a = tokio::time::timeout(Duration::from_secs(5), rx_a.recv())
        .await
        .expect("A: no local frame within 5s")
        .expect("A: channel closed");
    let va: Value = serde_json::from_str(&frame_a).expect("A: frame is json");
    assert_eq!(va["evt"], "presence.state", "A: evt mismatch");

    // ...and NOT a second time from its own Redis echo (self-drop).
    let second = tokio::time::timeout(Duration::from_millis(1500), rx_a.recv()).await;
    assert!(
        second.is_err(),
        "A: received its own event twice — self-drop failed"
    );
}
