//! Sprint 4 receipts + typing integration tests (OPN-CORE.md §10.2): the
//! delivered/read watermark fan-out and the ephemeral typing ping, driven end
//! to end over a live socket via `common::ws`. Watermark semantics under test:
//! monotonic + clamped to the channel's `last_seq`, a receipt event only on a
//! real advance (regress/repeat is a silent idempotent OK), delivered and read
//! are independent watermarks, and both mark and typing are membership-gated.

mod common;

use std::time::Duration;

use common::ws::{connect_and_auth, mint_full, spawn_server, TestClient};
use common::{app_pool, seed_world_tenant, test_config, test_state};
use opn_core::infra::ids::new_id;
use serde_json::{json, Value};
use sqlx::PgPool;

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

/// `channels.mark_read` / `mark_delivered` — returns the raw ack.
async fn mark(c: &mut TestClient, cmd: &str, channel_id: &str, up_to_seq: i64) -> Value {
    c.cmd(json!({ "cmd": cmd, "payload": {
        "channel_id": channel_id,
        "up_to_seq": up_to_seq,
    } }))
    .await
}

/// `channels.typing` — returns the raw ack.
async fn typing(c: &mut TestClient, channel_id: &str) -> Value {
    c.cmd(json!({ "cmd": "channels.typing", "payload": { "channel_id": channel_id } }))
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

// 1. A watermark advance emits a `channels.receipt` to the `ch:` subscriber; a
//    regress/repeat is a silent OK; a mark past `last_seq` clamps to it.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn receipts_monotonic_and_emit(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 16).await;
    let (token_a, a) = mint_full(&app, tenant, world, "a").await;
    let (token_b, b) = mint_full(&app, tenant, world, "b").await;
    let server = spawn_server(test_state(app, test_config()).await).await;

    let mut ca = connect_and_auth(server.addr, &token_a).await;
    let char_a = a.identity.character_id;
    let number_b = b.character.number.clone().expect("B has a number");
    let channel_id = cid(&open_direct(&mut ca, &number_b).await);

    // A sends 3 (Msg burst 5, so 3 rapid sends are fine) → channel last_seq = 3.
    for i in 1..=3 {
        let ack = send_text(&mut ca, &channel_id, &format!("m{i}")).await;
        assert_eq!(ack["ok"], json!(true), "send {i}: {ack}");
    }

    // B subs the channel — no `last_seq` in the sub, so no resume replay
    // buffers `channels.message` events ahead of the receipt under test.
    let mut cb = connect_and_auth(server.addr, &token_b).await;
    let ack = cb.cmd(sub(format!("ch:{channel_id}"))).await;
    assert_eq!(ack["ok"], json!(true), "B sub: {ack}");

    // A marks read up_to 2 → the watermark advances → B receives one receipt.
    let ack = mark(&mut ca, "channels.mark_read", &channel_id, 2).await;
    assert_eq!(ack["ok"], json!(true), "mark read 2: {ack}");

    let ev = cb.expect_evt(EVT_WAIT).await;
    assert_eq!(ev["evt"], json!("channels.receipt"), "evt: {ev}");
    assert_eq!(
        ev["topic"],
        json!(format!("ch:{channel_id}")),
        "topic: {ev}"
    );
    assert_eq!(ev["payload"]["channel_id"], json!(channel_id), "chan: {ev}");
    assert_eq!(ev["payload"]["character_id"], json!(char_a), "who: {ev}");
    assert_eq!(ev["payload"]["kind"], json!("read"), "kind: {ev}");
    assert_eq!(ev["payload"]["up_to_seq"], json!(2), "seq: {ev}");
    assert!(
        ev["payload"]["at"].as_str().is_some_and(|s| !s.is_empty()),
        "at is a non-empty rfc3339 string: {ev}"
    );

    // Regress: mark read up_to 1 → idempotent OK, but NO event fans out.
    let ack = mark(&mut ca, "channels.mark_read", &channel_id, 1).await;
    assert_eq!(ack["ok"], json!(true), "regress mark ok: {ack}");
    cb.expect_no_evt(Duration::from_millis(300)).await;

    // Beyond last_seq: mark read up_to 99 → clamps to 3 → receipt carries 3.
    let ack = mark(&mut ca, "channels.mark_read", &channel_id, 99).await;
    assert_eq!(ack["ok"], json!(true), "mark read 99: {ack}");

    let ev = cb.expect_evt(EVT_WAIT).await;
    assert_eq!(ev["evt"], json!("channels.receipt"), "evt2: {ev}");
    assert_eq!(ev["payload"]["kind"], json!("read"), "kind2: {ev}");
    assert_eq!(
        ev["payload"]["up_to_seq"],
        json!(3),
        "clamped to last_seq: {ev}"
    );
}

// 2. `delivered` and `read` are independent watermarks; a non-member marking is
//    `forbidden`.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn receipts_both_kinds(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 16).await;
    let (token_a, _a) = mint_full(&app, tenant, world, "a").await;
    let (_token_b, b) = mint_full(&app, tenant, world, "b").await;
    let (token_c, _c) = mint_full(&app, tenant, world, "c").await;
    let server = spawn_server(test_state(app, test_config()).await).await;

    let mut ca = connect_and_auth(server.addr, &token_a).await;
    let number_b = b.character.number.clone().expect("B number");
    let channel_id = cid(&open_direct(&mut ca, &number_b).await);

    // Messages so a watermark of 2 sits inside last_seq (= 3).
    for i in 1..=3 {
        let ack = send_text(&mut ca, &channel_id, &format!("m{i}")).await;
        assert_eq!(ack["ok"], json!(true), "send {i}: {ack}");
    }

    // Advance delivered to 2; read is never marked, so it stays 0.
    let ack = mark(&mut ca, "channels.mark_delivered", &channel_id, 2).await;
    assert_eq!(ack["ok"], json!(true), "mark delivered 2: {ack}");

    // A fresh list ack proves the two watermarks moved independently.
    let ack = ca.cmd(json!({ "cmd": "channels.list" })).await;
    let ch = channel_in_list(&ack, &channel_id).expect("A lists the DM");
    assert_eq!(
        ch["last_delivered_seq"],
        json!(2),
        "delivered advanced: {ch}"
    );
    assert_eq!(ch["last_read_seq"], json!(0), "read untouched: {ch}");

    // C is a character of the same world but not in the DM → marking forbidden.
    let mut cc = connect_and_auth(server.addr, &token_c).await;
    let ack = mark(&mut cc, "channels.mark_read", &channel_id, 1).await;
    assert_eq!(ack["err"]["code"], json!("forbidden"), "C mark_read: {ack}");
}

// 3. Typing fans out an ephemeral `channels.typing` to the `ch:` subscriber; a
//    non-member typing is `forbidden`.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn typing_delivered_and_authz(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 16).await;
    let (token_a, a) = mint_full(&app, tenant, world, "a").await;
    let (token_b, b) = mint_full(&app, tenant, world, "b").await;
    let (token_c, _c) = mint_full(&app, tenant, world, "c").await;
    let server = spawn_server(test_state(app, test_config()).await).await;

    let mut ca = connect_and_auth(server.addr, &token_a).await;
    let char_a = a.identity.character_id;
    let number_b = b.character.number.clone().expect("B number");
    let channel_id = cid(&open_direct(&mut ca, &number_b).await);

    let mut cb = connect_and_auth(server.addr, &token_b).await;
    let ack = cb.cmd(sub(format!("ch:{channel_id}"))).await;
    assert_eq!(ack["ok"], json!(true), "B sub: {ack}");

    // A types → B receives the ephemeral ping tagged with A's character.
    let ack = typing(&mut ca, &channel_id).await;
    assert_eq!(ack["ok"], json!(true), "typing ack: {ack}");

    let ev = cb.expect_evt(EVT_WAIT).await;
    assert_eq!(ev["evt"], json!("channels.typing"), "evt: {ev}");
    assert_eq!(
        ev["topic"],
        json!(format!("ch:{channel_id}")),
        "topic: {ev}"
    );
    assert_eq!(ev["payload"]["channel_id"], json!(channel_id), "chan: {ev}");
    assert_eq!(ev["payload"]["character_id"], json!(char_a), "who: {ev}");

    // C (not a member) typing into the same channel → forbidden.
    let mut cc = connect_and_auth(server.addr, &token_c).await;
    let ack = typing(&mut cc, &channel_id).await;
    assert_eq!(ack["err"]["code"], json!("forbidden"), "C typing: {ack}");
}
