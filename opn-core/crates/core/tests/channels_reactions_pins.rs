//! Sprint 4 channels reactions + pins integration tests (OPN-CORE.md §10.2):
//! react/unreact and pin/unpin over the live socket — the change-only event
//! rule, membership/existence authz, emoji validation, and the 50-pin cap
//! (enforced under the channel row lock). Message rows are seeded directly via
//! `world_tx` (WS sends are rate-limited ~1/s), the same escape hatch
//! `channels.rs::offline_member_gets_inbox_alert` uses for inbox reads.

mod common;

use std::time::Duration;

use common::ws::{connect_and_auth, mint_full, spawn_server, TestClient};
use common::{app_pool, seed_world_tenant, test_config, test_state};
use opn_core::infra::db::world_tx;
use opn_core::infra::ids::new_id;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

const EVT_WAIT: Duration = Duration::from_secs(2);
/// Negative-presence wait: the handler publishes inline before its ack, so a
/// no-change command has already decided not to publish by the time we poll.
const NO_EVT_WAIT: Duration = Duration::from_millis(500);

fn sub(topic: String) -> Value {
    json!({ "cmd": "sub", "payload": { "topic": topic } })
}

/// `channels.open_direct` → the ack.
async fn open_direct(c: &mut TestClient, number: &str) -> Value {
    c.cmd(json!({ "cmd": "channels.open_direct", "payload": { "number": number } }))
        .await
}

/// Pull `payload.channel_id` off an ack, panicking with the ack on absence.
fn cid(ack: &Value) -> String {
    ack["payload"]["channel_id"]
        .as_str()
        .unwrap_or_else(|| panic!("no channel_id: {ack}"))
        .to_string()
}

fn react_cmd(channel_id: &str, message_id: &str, emoji: &str, add: bool) -> Value {
    json!({
        "cmd": if add { "channels.react" } else { "channels.unreact" },
        "payload": { "channel_id": channel_id, "message_id": message_id, "emoji": emoji },
    })
}

fn pin_cmd(channel_id: &str, message_id: &str, add: bool) -> Value {
    json!({
        "cmd": if add { "channels.pin" } else { "channels.unpin" },
        "payload": { "channel_id": channel_id, "message_id": message_id },
    })
}

/// Insert one `messages` row directly (bypassing the WS rate limit) and bump
/// the channel's `last_seq`. Returns the new message id. `messages` is
/// partitioned by `created_at`; the default (current month) partition exists,
/// so a defaulted `created_at` lands fine.
async fn seed_message(app: &PgPool, world: Uuid, channel_id: Uuid, seq: i64, sender: Uuid) -> Uuid {
    let msg_id = new_id();
    let mut tx = world_tx(app, world).await.expect("world_tx");
    sqlx::query(
        "INSERT INTO messages \
           (id, world_id, channel_id, seq, sender_character, body, client_uuid) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(msg_id)
    .bind(world)
    .bind(channel_id)
    .bind(seq)
    .bind(sender)
    .bind(json!({ "text": "x" }))
    .bind(new_id())
    .execute(&mut *tx)
    .await
    .expect("insert message");
    sqlx::query("UPDATE channels SET last_seq = GREATEST(last_seq, $2) WHERE id = $1")
        .bind(channel_id)
        .bind(seq)
        .execute(&mut *tx)
        .await
        .expect("bump last_seq");
    tx.commit().await.expect("commit seed_message");
    msg_id
}

/// Seed `n` pin rows directly (no FK to `messages`, so arbitrary message ids
/// are fine) to preload the channel toward the 50 cap. `pinned_by` must be a
/// real character of `world`.
async fn seed_pins(app: &PgPool, world: Uuid, channel_id: Uuid, by: Uuid, n: usize) {
    let mut tx = world_tx(app, world).await.expect("world_tx");
    for _ in 0..n {
        sqlx::query(
            "INSERT INTO channel_pins (channel_id, world_id, message_id, pinned_by) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(channel_id)
        .bind(world)
        .bind(new_id())
        .bind(by)
        .execute(&mut *tx)
        .await
        .expect("insert pin");
    }
    tx.commit().await.expect("commit seed_pins");
}

async fn pin_count(app: &PgPool, world: Uuid, channel_id: Uuid) -> i64 {
    let mut tx = world_tx(app, world).await.expect("world_tx");
    sqlx::query_scalar("SELECT count(*) FROM channel_pins WHERE channel_id = $1")
        .bind(channel_id)
        .fetch_one(&mut *tx)
        .await
        .expect("count pins")
}

// react/unreact: change-only event, membership + message-in-channel authz, and
// emoji validation (§10.2).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn react_add_remove_and_authz(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 16).await;
    let (token_a, a) = mint_full(&app, tenant, world, "a").await;
    let (token_b, b) = mint_full(&app, tenant, world, "b").await;
    let (token_c, _c) = mint_full(&app, tenant, world, "c").await;
    let char_a = a.identity.character_id;
    let server = spawn_server(test_state(app.clone(), test_config()).await).await;

    // A opens the DM, then a message is seeded into it to react to.
    let mut ca = connect_and_auth(server.addr, &token_a).await;
    let number_b = b.character.number.clone().expect("B number");
    let channel_id = cid(&open_direct(&mut ca, &number_b).await);
    let chan = Uuid::parse_str(&channel_id).expect("channel_id is a uuid");
    let msg_id = seed_message(&app, world, chan, 1, char_a).await;

    // B subs so it observes the live reaction fan-out.
    let mut cb = connect_and_auth(server.addr, &token_b).await;
    let ack = cb.cmd(sub(format!("ch:{channel_id}"))).await;
    assert_eq!(ack["ok"], json!(true), "B sub: {ack}");

    // First 👍 → a real change → B sees added=true.
    let ack = ca
        .cmd(react_cmd(&channel_id, &msg_id.to_string(), "👍", true))
        .await;
    assert_eq!(ack["ok"], json!(true), "react add: {ack}");
    let ev = cb.expect_evt(EVT_WAIT).await;
    assert_eq!(ev["evt"], json!("channels.reaction"), "evt: {ev}");
    assert_eq!(
        ev["topic"],
        json!(format!("ch:{channel_id}")),
        "topic: {ev}"
    );
    assert_eq!(ev["payload"]["channel_id"], json!(channel_id), "chan: {ev}");
    assert_eq!(ev["payload"]["message_id"], json!(msg_id), "msg: {ev}");
    assert_eq!(ev["payload"]["character_id"], json!(char_a), "who: {ev}");
    assert_eq!(ev["payload"]["emoji"], json!("👍"), "emoji: {ev}");
    assert_eq!(ev["payload"]["added"], json!(true), "added: {ev}");

    // Duplicate 👍 → OK ack, NO event.
    let ack = ca
        .cmd(react_cmd(&channel_id, &msg_id.to_string(), "👍", true))
        .await;
    assert_eq!(ack["ok"], json!(true), "dup react: {ack}");
    cb.expect_no_evt(NO_EVT_WAIT).await;

    // Unreact 👍 → a real change → B sees added=false.
    let ack = ca
        .cmd(react_cmd(&channel_id, &msg_id.to_string(), "👍", false))
        .await;
    assert_eq!(ack["ok"], json!(true), "unreact: {ack}");
    let ev = cb.expect_evt(EVT_WAIT).await;
    assert_eq!(ev["evt"], json!("channels.reaction"), "evt: {ev}");
    assert_eq!(ev["payload"]["added"], json!(false), "removed: {ev}");

    // Non-member C → forbidden.
    let mut cc = connect_and_auth(server.addr, &token_c).await;
    let ack = cc
        .cmd(react_cmd(&channel_id, &msg_id.to_string(), "👍", true))
        .await;
    assert_eq!(ack["err"]["code"], json!("forbidden"), "C react: {ack}");

    // Message not in this channel → not_found.
    let ghost = new_id();
    let ack = ca
        .cmd(react_cmd(&channel_id, &ghost.to_string(), "👍", true))
        .await;
    assert_eq!(ack["err"]["code"], json!("not_found"), "ghost msg: {ack}");

    // Invalid emoji: empty, and >8 bytes.
    let ack = ca
        .cmd(react_cmd(&channel_id, &msg_id.to_string(), "", true))
        .await;
    assert_eq!(ack["err"]["code"], json!("invalid"), "empty emoji: {ack}");
    let ack = ca
        .cmd(react_cmd(
            &channel_id,
            &msg_id.to_string(),
            "abcdefghij",
            true,
        ))
        .await;
    assert_eq!(ack["err"]["code"], json!("invalid"), "long emoji: {ack}");
}

// The 50-pin cap holds under a concurrent race at 49: two pins for two distinct
// messages fire together; the channel row lock serializes them, so exactly one
// commits (→ 50) and the other sees `conflict`. Count settles at exactly 50.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn pins_cap_50(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 16).await;
    let (token_a, a) = mint_full(&app, tenant, world, "a").await;
    let (token_b, b) = mint_full(&app, tenant, world, "b").await;
    let char_a = a.identity.character_id;
    let server = spawn_server(test_state(app.clone(), test_config()).await).await;

    let mut ca = connect_and_auth(server.addr, &token_a).await;
    let number_b = b.character.number.clone().expect("B number");
    let channel_id = cid(&open_direct(&mut ca, &number_b).await);
    let chan = Uuid::parse_str(&channel_id).expect("channel_id is a uuid");

    // Preload 49 pins, then two real messages to race the 50th/51st pin on.
    seed_pins(&app, world, chan, char_a, 49).await;
    let m50 = seed_message(&app, world, chan, 50, char_a).await;
    let m51 = seed_message(&app, world, chan, 51, char_a).await;
    assert_eq!(pin_count(&app, world, chan).await, 49, "preloaded 49 pins");

    // Two concurrent pins from the DM's two members (distinct sessions — two
    // `token_a` connections would evict each other, last-writer-wins §4.1). The
    // FOR UPDATE lock on the channel row is the serialization point: one wins
    // the last slot.
    let mut cb = connect_and_auth(server.addr, &token_b).await;
    let (r1, r2) = tokio::join!(
        ca.cmd(pin_cmd(&channel_id, &m50.to_string(), true)),
        cb.cmd(pin_cmd(&channel_id, &m51.to_string(), true)),
    );

    let acks = [&r1, &r2];
    let ok = acks.iter().filter(|a| a["ok"] == json!(true)).count();
    let conflict = acks
        .iter()
        .filter(|a| a["err"]["code"] == json!("conflict"))
        .count();
    assert_eq!(ok, 1, "exactly one pin succeeds: {r1} | {r2}");
    assert_eq!(conflict, 1, "exactly one pin conflicts: {r1} | {r2}");
    assert_eq!(
        pin_count(&app, world, chan).await,
        50,
        "cap holds at exactly 50"
    );
}

// pin/unpin round trip: change-only event both ways, no-op is silent, and a
// non-member is forbidden (§10.2).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn pin_unpin_roundtrip(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 16).await;
    let (token_a, a) = mint_full(&app, tenant, world, "a").await;
    let (token_b, b) = mint_full(&app, tenant, world, "b").await;
    let (token_c, _c) = mint_full(&app, tenant, world, "c").await;
    let char_a = a.identity.character_id;
    let server = spawn_server(test_state(app.clone(), test_config()).await).await;

    let mut ca = connect_and_auth(server.addr, &token_a).await;
    let number_b = b.character.number.clone().expect("B number");
    let channel_id = cid(&open_direct(&mut ca, &number_b).await);
    let chan = Uuid::parse_str(&channel_id).expect("channel_id is a uuid");
    let msg_id = seed_message(&app, world, chan, 1, char_a).await;

    let mut cb = connect_and_auth(server.addr, &token_b).await;
    let ack = cb.cmd(sub(format!("ch:{channel_id}"))).await;
    assert_eq!(ack["ok"], json!(true), "B sub: {ack}");

    // Pin → a real change → B sees pinned=true.
    let ack = ca
        .cmd(pin_cmd(&channel_id, &msg_id.to_string(), true))
        .await;
    assert_eq!(ack["ok"], json!(true), "pin: {ack}");
    let ev = cb.expect_evt(EVT_WAIT).await;
    assert_eq!(ev["evt"], json!("channels.pin"), "evt: {ev}");
    assert_eq!(
        ev["topic"],
        json!(format!("ch:{channel_id}")),
        "topic: {ev}"
    );
    assert_eq!(ev["payload"]["channel_id"], json!(channel_id), "chan: {ev}");
    assert_eq!(ev["payload"]["message_id"], json!(msg_id), "msg: {ev}");
    assert_eq!(ev["payload"]["by"], json!(char_a), "by: {ev}");
    assert_eq!(ev["payload"]["pinned"], json!(true), "pinned: {ev}");

    // Pin again → OK ack, NO event.
    let ack = ca
        .cmd(pin_cmd(&channel_id, &msg_id.to_string(), true))
        .await;
    assert_eq!(ack["ok"], json!(true), "dup pin: {ack}");
    cb.expect_no_evt(NO_EVT_WAIT).await;

    // Unpin → a real change → B sees pinned=false.
    let ack = ca
        .cmd(pin_cmd(&channel_id, &msg_id.to_string(), false))
        .await;
    assert_eq!(ack["ok"], json!(true), "unpin: {ack}");
    let ev = cb.expect_evt(EVT_WAIT).await;
    assert_eq!(ev["evt"], json!("channels.pin"), "evt: {ev}");
    assert_eq!(ev["payload"]["pinned"], json!(false), "unpinned: {ev}");

    // Unpin again → OK ack, NO event.
    let ack = ca
        .cmd(pin_cmd(&channel_id, &msg_id.to_string(), false))
        .await;
    assert_eq!(ack["ok"], json!(true), "dup unpin: {ack}");
    cb.expect_no_evt(NO_EVT_WAIT).await;

    // Non-member C → forbidden.
    let mut cc = connect_and_auth(server.addr, &token_c).await;
    let ack = cc
        .cmd(pin_cmd(&channel_id, &msg_id.to_string(), true))
        .await;
    assert_eq!(ack["err"]["code"], json!("forbidden"), "C pin: {ack}");
}
