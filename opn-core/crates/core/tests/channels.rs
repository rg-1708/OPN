//! Sprint 3 channels/messaging integration tests (OPN-CORE.md §8, §10.2):
//! the send fan-out end to end, open_direct found-or-create, group create +
//! list, membership authz, cross-world RLS isolation, and the offline-member
//! inbox alert/silent split. Every test drives the real router over a live
//! socket via `common::ws`. Concurrency/idempotency invariants live in
//! `channels_seq.rs`.

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

fn sub(topic: String) -> Value {
    json!({ "cmd": "sub", "payload": { "topic": topic } })
}

/// `channels.open_direct` — returns the raw ack.
async fn open_direct(c: &mut TestClient, number: &str) -> Value {
    c.cmd(json!({ "cmd": "channels.open_direct", "payload": { "number": number } }))
        .await
}

/// `channels.send` a text body with a fresh idempotency key — returns the ack.
async fn send_text(c: &mut TestClient, channel_id: &str, text: &str) -> Value {
    c.cmd(json!({ "cmd": "channels.send", "payload": {
        "channel_id": channel_id,
        "client_uuid": new_id(),
        "body": { "text": text },
    } }))
    .await
}

/// Pull `payload.channel_id` off an ack, panicking with the ack on absence.
fn cid(ack: &Value) -> String {
    ack["payload"]["channel_id"]
        .as_str()
        .unwrap_or_else(|| panic!("no channel_id: {ack}"))
        .to_string()
}

/// The summary for `channel_id` in a `channels.list` ack payload, if present.
fn channel_in_list<'a>(ack: &'a Value, channel_id: &str) -> Option<&'a Value> {
    ack["payload"]
        .as_array()?
        .iter()
        .find(|c| c["channel_id"] == json!(channel_id))
}

/// Poll `character`'s inbox (fan-out is async, off the send hot path) until it
/// holds `>= n` rows; returns the `class` column newest-first. Bounded so a
/// missing row is a failure, not a hang.
async fn poll_inbox(app: &PgPool, world: Uuid, character: Uuid, n: usize) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let mut tx = world_tx(app, world).await.expect("world_tx");
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT class FROM inbox WHERE character_id = $1 ORDER BY created_at DESC, id DESC",
        )
        .bind(character)
        .fetch_all(&mut *tx)
        .await
        .expect("read inbox");
        drop(tx); // read-only: roll back, release the connection before we sleep
        if rows.len() >= n {
            return rows;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "inbox never reached {n} rows: {rows:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// 1. The end-to-end demo: persist → ack → live fan-out (§8). A opens a DM to
//    B, B subs the channel, A sends, B receives the push.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn send_delivers_to_subscriber(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 16).await;
    let (token_a, _a) = mint_full(&app, tenant, world, "a").await;
    let (token_b, b) = mint_full(&app, tenant, world, "b").await;
    let server = spawn_server(test_state(app, test_config()).await).await;

    let mut ca = connect_and_auth(server.addr, &token_a).await;
    let number_b = b.character.number.clone().expect("B has a number");
    let ack = open_direct(&mut ca, &number_b).await;
    assert_eq!(ack["ok"], json!(true), "open_direct: {ack}");
    let channel_id = cid(&ack);

    // B must be a member to sub (§4.4).
    let mut cb = connect_and_auth(server.addr, &token_b).await;
    let ack = cb.cmd(sub(format!("ch:{channel_id}"))).await;
    assert_eq!(ack["ok"], json!(true), "B sub: {ack}");

    let ack = send_text(&mut ca, &channel_id, "hi").await;
    assert_eq!(ack["ok"], json!(true), "send: {ack}");
    assert!(
        ack["payload"]["message_id"].is_string(),
        "message_id: {ack}"
    );
    assert_eq!(ack["payload"]["seq"], json!(1), "first seq: {ack}");

    let ev = cb.expect_evt(EVT_WAIT).await;
    assert_eq!(ev["evt"], json!("channels.message"), "evt: {ev}");
    assert_eq!(
        ev["topic"],
        json!(format!("ch:{channel_id}")),
        "topic: {ev}"
    );
    assert_eq!(ev["payload"]["channel_id"], json!(channel_id), "chan: {ev}");
    assert_eq!(ev["payload"]["seq"], json!(1), "push seq: {ev}");
    assert_eq!(ev["payload"]["body"]["text"], json!("hi"), "body: {ev}");
}

// 2. open_direct is found-or-create and order-independent; unknown → not_found,
//    self → invalid.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn open_direct_found_or_create(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 16).await;
    let (token_a, a) = mint_full(&app, tenant, world, "a").await;
    let (token_b, b) = mint_full(&app, tenant, world, "b").await;
    let server = spawn_server(test_state(app, test_config()).await).await;

    let number_a = a.character.number.clone().expect("A number");
    let number_b = b.character.number.clone().expect("B number");
    let mut ca = connect_and_auth(server.addr, &token_a).await;
    let mut cb = connect_and_auth(server.addr, &token_b).await;

    // Twice from A → the same channel (found, not re-created).
    let c1 = cid(&open_direct(&mut ca, &number_b).await);
    let c2 = cid(&open_direct(&mut ca, &number_b).await);
    assert_eq!(c1, c2, "A→B twice converges");

    // B→A → the same ordered-pair row, order-independent.
    let c3 = cid(&open_direct(&mut cb, &number_a).await);
    assert_eq!(c1, c3, "B→A is the same pair");

    // Unknown number (never a 555-XXXX assignment) → not_found.
    let ack = open_direct(&mut ca, "000-0000").await;
    assert_eq!(ack["err"]["code"], json!("not_found"), "unknown: {ack}");

    // Own number → invalid (no self-DM).
    let ack = open_direct(&mut ca, &number_a).await;
    assert_eq!(ack["err"]["code"], json!("invalid"), "self: {ack}");
}

// 3. Group create + list: the creator and an added member both see it.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn create_group_and_list(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 16).await;
    let (token_a, _a) = mint_full(&app, tenant, world, "a").await;
    let (token_b, b) = mint_full(&app, tenant, world, "b").await;
    let (_token_c, c) = mint_full(&app, tenant, world, "c").await;
    let server = spawn_server(test_state(app, test_config()).await).await;

    let mut ca = connect_and_auth(server.addr, &token_a).await;
    let ack = ca
        .cmd(json!({ "cmd": "channels.create", "payload": {
            "name": "g",
            "members": [b.identity.character_id, c.identity.character_id],
        } }))
        .await;
    assert_eq!(ack["ok"], json!(true), "create: {ack}");
    let channel_id = cid(&ack);

    let ack = ca.cmd(json!({ "cmd": "channels.list" })).await;
    let ch = channel_in_list(&ack, &channel_id).expect("A lists the group");
    assert_eq!(ch["kind"], json!("group"), "kind: {ch}");
    assert_eq!(ch["name"], json!("g"), "name: {ch}");

    // B is a member, so B's list carries it too.
    let mut cb = connect_and_auth(server.addr, &token_b).await;
    let ack = cb.cmd(json!({ "cmd": "channels.list" })).await;
    assert!(
        channel_in_list(&ack, &channel_id).is_some(),
        "B lists the group: {ack}"
    );
}

// 4. Member cap is 32: a 33-member create is rejected before it touches SQL.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn create_cap_rejected(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let (token_a, _a) = mint_full(&app, tenant, world, "a").await;
    let server = spawn_server(test_state(app, test_config()).await).await;

    let mut ca = connect_and_auth(server.addr, &token_a).await;
    let members: Vec<Uuid> = (0..33).map(|_| new_id()).collect();
    let ack = ca
        .cmd(json!({ "cmd": "channels.create", "payload": { "name": null, "members": members } }))
        .await;
    assert_eq!(ack["err"]["code"], json!("invalid"), "over cap: {ack}");
}

// 5. A group member must live in the caller's world; a foreign id → invalid
//    (RLS hides it, so the count check falls short).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn create_cross_world_member_rejected(admin: PgPool) {
    let (world1, tenant1, _) = seed_world_tenant(&admin).await;
    let (world2, tenant2, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let (token_a, _a) = mint_full(&app, tenant1, world1, "a").await;
    let (_token_x, x) = mint_full(&app, tenant2, world2, "x").await;
    let server = spawn_server(test_state(app, test_config()).await).await;

    let mut ca = connect_and_auth(server.addr, &token_a).await;
    let ack = ca
        .cmd(json!({ "cmd": "channels.create", "payload": {
            "name": null,
            "members": [x.identity.character_id],
        } }))
        .await;
    assert_eq!(
        ack["err"]["code"],
        json!("invalid"),
        "foreign member: {ack}"
    );
}

// 6. A non-member can neither send to nor sub a channel → forbidden both ways.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn non_member_send_forbidden(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 16).await;
    let (token_a, _a) = mint_full(&app, tenant, world, "a").await;
    let (_token_b, b) = mint_full(&app, tenant, world, "b").await;
    let (token_c, _c) = mint_full(&app, tenant, world, "c").await;
    let server = spawn_server(test_state(app, test_config()).await).await;

    // A opens a DM with B; C is in neither seat.
    let mut ca = connect_and_auth(server.addr, &token_a).await;
    let number_b = b.character.number.clone().expect("B number");
    let channel_id = cid(&open_direct(&mut ca, &number_b).await);

    let mut cc = connect_and_auth(server.addr, &token_c).await;
    let ack = send_text(&mut cc, &channel_id, "intrude").await;
    assert_eq!(ack["err"]["code"], json!("forbidden"), "send: {ack}");

    let ack = cc.cmd(sub(format!("ch:{channel_id}"))).await;
    assert_eq!(ack["err"]["code"], json!("forbidden"), "sub: {ack}");
}

// 7. A channel is invisible cross-world: a character in another world sees the
//    same channel_id as forbidden (RLS makes non-member indistinguishable).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn cross_world_channel_isolated(admin: PgPool) {
    let (world1, tenant1, _) = seed_world_tenant(&admin).await;
    let (world2, tenant2, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 16).await;
    let (token_a, _a) = mint_full(&app, tenant1, world1, "a").await;
    let (_token_b, b) = mint_full(&app, tenant1, world1, "b").await;
    let (token_x, _x) = mint_full(&app, tenant2, world2, "x").await;
    let server = spawn_server(test_state(app, test_config()).await).await;

    // Channel lives in world1.
    let mut ca = connect_and_auth(server.addr, &token_a).await;
    let number_b = b.character.number.clone().expect("B number");
    let channel_id = cid(&open_direct(&mut ca, &number_b).await);

    // X (world2) using that id sees only forbidden, for sub and send alike.
    let mut cx = connect_and_auth(server.addr, &token_x).await;
    let ack = cx.cmd(sub(format!("ch:{channel_id}"))).await;
    assert_eq!(ack["err"]["code"], json!("forbidden"), "x sub: {ack}");
    let ack = send_text(&mut cx, &channel_id, "peek").await;
    assert_eq!(ack["err"]["code"], json!("forbidden"), "x send: {ack}");
}

// 8. An offline member gets an inbox row, not a live push: class `alert` when
//    unmuted, downgraded to `silent` when the membership is muted (§10.8).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn offline_member_gets_inbox_alert(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 16).await;
    let (token_a, _a) = mint_full(&app, tenant, world, "a").await;
    let (_token_b, b) = mint_full(&app, tenant, world, "b").await;
    let char_b = b.identity.character_id;
    // Keep `app` for direct inbox reads; the server gets a shared clone.
    let server = spawn_server(test_state(app.clone(), test_config()).await).await;

    // A opens the DM; B never connects (offline).
    let mut ca = connect_and_auth(server.addr, &token_a).await;
    let number_b = b.character.number.clone().expect("B number");
    let channel_id = cid(&open_direct(&mut ca, &number_b).await);

    let ack = send_text(&mut ca, &channel_id, "first").await;
    assert_eq!(ack["ok"], json!(true), "send 1: {ack}");
    let classes = poll_inbox(&app, world, char_b, 1).await;
    assert_eq!(classes[0], "alert", "unmuted → alert: {classes:?}");

    // Mute B's membership, then a fresh send inboxes a silent row.
    let mut tx = world_tx(&app, world).await.expect("world_tx");
    sqlx::query(
        "UPDATE channel_members SET muted = true WHERE channel_id = $1 AND character_id = $2",
    )
    .bind(Uuid::parse_str(&channel_id).expect("channel_id is a uuid"))
    .bind(char_b)
    .execute(&mut *tx)
    .await
    .expect("mute B");
    tx.commit().await.expect("commit mute");

    let ack = send_text(&mut ca, &channel_id, "second").await;
    assert_eq!(ack["ok"], json!(true), "send 2: {ack}");
    let classes = poll_inbox(&app, world, char_b, 2).await;
    assert_eq!(classes[0], "silent", "muted → silent (newest): {classes:?}");
}
