//! Sprint 4 channels integration tests: group membership mutation
//! (`member_add`/`member_remove`, §10.2) and `sub` resume replay (§4.4).
//! Every test drives the real router over a live socket via `common::ws`;
//! resume messages are seeded straight into `messages` via `world_tx` (the WS
//! send path is rate-limited ~1/s, so a bulk seed can't ride it).

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
const SHORT: Duration = Duration::from_millis(400);

/// Plain `sub` (no resume watermark).
fn sub(topic: String) -> Value {
    json!({ "cmd": "sub", "payload": { "topic": topic } })
}

/// `sub` with a resume watermark — triggers replay of `seq > last_seq`.
fn sub_resume(channel_id: &str, last_seq: i64) -> Value {
    json!({ "cmd": "sub", "payload": { "topic": format!("ch:{channel_id}"), "last_seq": last_seq } })
}

async fn open_direct(c: &mut TestClient, number: &str) -> Value {
    c.cmd(json!({ "cmd": "channels.open_direct", "payload": { "number": number } }))
        .await
}

async fn member_add(c: &mut TestClient, channel_id: &str, character_id: Uuid) -> Value {
    c.cmd(json!({ "cmd": "channels.member_add", "payload": {
        "channel_id": channel_id,
        "character_id": character_id,
    } }))
    .await
}

async fn member_remove(c: &mut TestClient, channel_id: &str, character_id: Uuid) -> Value {
    c.cmd(json!({ "cmd": "channels.member_remove", "payload": {
        "channel_id": channel_id,
        "character_id": character_id,
    } }))
    .await
}

async fn send_text(c: &mut TestClient, channel_id: &str, text: &str) -> Value {
    c.cmd(json!({ "cmd": "channels.send", "payload": {
        "channel_id": channel_id,
        "client_uuid": new_id(),
        "body": { "text": text },
    } }))
    .await
}

fn cid(ack: &Value) -> String {
    ack["payload"]["channel_id"]
        .as_str()
        .unwrap_or_else(|| panic!("no channel_id: {ack}"))
        .to_string()
}

/// Insert `count` messages (seq `1..=count`) straight into `channel`, then bump
/// the channel's `last_seq`. Mirrors the send path's rows without the WS rate
/// limit, so a resume gap can be arbitrarily wide.
async fn seed_messages(app: &PgPool, world: Uuid, channel: Uuid, sender: Uuid, count: i64) {
    let mut tx = world_tx(app, world).await.expect("world_tx");
    for seq in 1..=count {
        sqlx::query(
            "INSERT INTO messages (id, world_id, channel_id, seq, sender_character, body, client_uuid) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(new_id())
        .bind(world)
        .bind(channel)
        .bind(seq)
        .bind(sender)
        .bind(json!({ "text": "m" }))
        .bind(new_id())
        .execute(&mut *tx)
        .await
        .expect("insert seed message");
    }
    sqlx::query("UPDATE channels SET last_seq = $2 WHERE id = $1")
        .bind(channel)
        .bind(count)
        .execute(&mut *tx)
        .await
        .expect("bump last_seq");
    tx.commit().await.expect("commit seed");
}

// 1. member_add/remove is group-only, member-gated, and existence-checked; a
//    real change fans a `channels.member` event to the channel's subscribers.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn member_add_remove_group_only(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 16).await;
    let (token_a, _a) = mint_full(&app, tenant, world, "a").await;
    let (token_b, b) = mint_full(&app, tenant, world, "b").await;
    let (_token_c, c) = mint_full(&app, tenant, world, "c").await;
    let (_token_d, d) = mint_full(&app, tenant, world, "d").await;
    let (token_e, _e) = mint_full(&app, tenant, world, "e").await;
    let server = spawn_server(test_state(app, test_config()).await).await;

    let char_b = b.identity.character_id;
    let char_c = c.identity.character_id;

    // A creates a group whose sole other member is B.
    let mut ca = connect_and_auth(server.addr, &token_a).await;
    let ack = ca
        .cmd(json!({ "cmd": "channels.create", "payload": {
            "name": "g",
            "members": [char_b],
        } }))
        .await;
    assert_eq!(ack["ok"], json!(true), "create group: {ack}");
    let group = cid(&ack);

    // B subscribes so it sees membership changes.
    let mut cb = connect_and_auth(server.addr, &token_b).await;
    let ack = cb.cmd(sub(format!("ch:{group}"))).await;
    assert_eq!(ack["ok"], json!(true), "B sub: {ack}");

    // A adds C → ok, B sees added=true for C.
    let ack = member_add(&mut ca, &group, char_c).await;
    assert_eq!(ack["ok"], json!(true), "add C: {ack}");
    let ev = cb.expect_evt(EVT_WAIT).await;
    assert_eq!(ev["evt"], json!("channels.member"), "add evt: {ev}");
    assert_eq!(ev["payload"]["channel_id"], json!(group), "add chan: {ev}");
    assert_eq!(
        ev["payload"]["character_id"],
        json!(char_c),
        "add who: {ev}"
    );
    assert_eq!(ev["payload"]["added"], json!(true), "add flag: {ev}");

    // A removes C → ok, B sees added=false for C.
    let ack = member_remove(&mut ca, &group, char_c).await;
    assert_eq!(ack["ok"], json!(true), "remove C: {ack}");
    let ev = cb.expect_evt(EVT_WAIT).await;
    assert_eq!(ev["evt"], json!("channels.member"), "remove evt: {ev}");
    assert_eq!(ev["payload"]["character_id"], json!(char_c), "rm who: {ev}");
    assert_eq!(ev["payload"]["added"], json!(false), "rm flag: {ev}");

    // On a DM channel, member mutation is a category error → conflict.
    let number_d = d.character.number.clone().expect("D number");
    let dm = cid(&open_direct(&mut ca, &number_d).await);
    let ack = member_add(&mut ca, &dm, char_c).await;
    assert_eq!(ack["err"]["code"], json!("conflict"), "add on dm: {ack}");
    let ack = member_remove(&mut ca, &dm, char_c).await;
    assert_eq!(ack["err"]["code"], json!("conflict"), "rm on dm: {ack}");

    // A non-member cannot mutate membership → forbidden.
    let mut ce = connect_and_auth(server.addr, &token_e).await;
    let ack = member_add(&mut ce, &group, char_c).await;
    assert_eq!(ack["err"]["code"], json!("forbidden"), "E add: {ack}");

    // Adding a character that does not exist in this world → invalid.
    let ack = member_add(&mut ca, &group, new_id()).await;
    assert_eq!(ack["err"]["code"], json!("invalid"), "ghost add: {ack}");
}

// 2. Removal drops the removed member's live subscription: they get the
//    added=false notice, then nothing further; a still-member keeps receiving.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn member_remove_drops_subscription(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 16).await;
    let (token_a, _a) = mint_full(&app, tenant, world, "a").await;
    let (token_b, b) = mint_full(&app, tenant, world, "b").await;
    let (token_c, c) = mint_full(&app, tenant, world, "c").await;
    let server = spawn_server(test_state(app, test_config()).await).await;

    let char_c = c.identity.character_id;

    // A creates a group with B and C.
    let mut ca = connect_and_auth(server.addr, &token_a).await;
    let ack = ca
        .cmd(json!({ "cmd": "channels.create", "payload": {
            "name": "g",
            "members": [b.identity.character_id, char_c],
        } }))
        .await;
    assert_eq!(ack["ok"], json!(true), "create group: {ack}");
    let group = cid(&ack);

    // C and B both subscribe.
    let mut cc = connect_and_auth(server.addr, &token_c).await;
    assert_eq!(
        cc.cmd(sub(format!("ch:{group}"))).await["ok"],
        json!(true),
        "C sub"
    );
    let mut cb = connect_and_auth(server.addr, &token_b).await;
    assert_eq!(
        cb.cmd(sub(format!("ch:{group}"))).await["ok"],
        json!(true),
        "B sub"
    );

    // A removes C. The removal fans to every subscriber of the topic, so both
    // C (its own exit notice, published before the drop) and B (still a
    // member) see the added=false event.
    let ack = member_remove(&mut ca, &group, char_c).await;
    assert_eq!(ack["ok"], json!(true), "remove C: {ack}");
    let ev = cc.expect_evt(EVT_WAIT).await;
    assert_eq!(ev["evt"], json!("channels.member"), "C notice evt: {ev}");
    assert_eq!(
        ev["payload"]["character_id"],
        json!(char_c),
        "C notice: {ev}"
    );
    assert_eq!(ev["payload"]["added"], json!(false), "C notice flag: {ev}");
    let ev = cb.expect_evt(EVT_WAIT).await;
    assert_eq!(ev["evt"], json!("channels.member"), "B notice evt: {ev}");
    assert_eq!(ev["payload"]["added"], json!(false), "B notice flag: {ev}");

    // A now sends to the group: B (still a member) receives it, C does not —
    // its subscription was dropped server-side on removal.
    let ack = send_text(&mut ca, &group, "after removal").await;
    assert_eq!(ack["ok"], json!(true), "send after removal: {ack}");
    let ev = cb.expect_evt(EVT_WAIT).await;
    assert_eq!(ev["evt"], json!("channels.message"), "B gets msg: {ev}");
    assert_eq!(
        ev["payload"]["body"]["text"],
        json!("after removal"),
        "B body: {ev}"
    );
    cc.expect_no_evt(SHORT).await;
}

// 3. A resume sub replays exactly the gap `seq > last_seq`, ascending, as
//    normal `channels.message` events before the sub ack — no overflow below cap.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn resume_replays_gap(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 16).await;
    let (token_a, a) = mint_full(&app, tenant, world, "a").await;
    let (token_b, b) = mint_full(&app, tenant, world, "b").await;
    let server = spawn_server(test_state(app.clone(), test_config()).await).await;

    // A opens the DM to B, then we seed 5 messages straight in.
    let mut ca = connect_and_auth(server.addr, &token_a).await;
    let number_b = b.character.number.clone().expect("B number");
    let channel_id = cid(&open_direct(&mut ca, &number_b).await);
    let channel = Uuid::parse_str(&channel_id).expect("channel_id uuid");
    seed_messages(&app, world, channel, a.identity.character_id, 5).await;

    // B resumes from seq 2 → replay 3,4,5 in order, buffered before the ack.
    let mut cb = connect_and_auth(server.addr, &token_b).await;
    let ack = cb.cmd(sub_resume(&channel_id, 2)).await;
    assert_eq!(ack["ok"], json!(true), "B resume sub: {ack}");

    for expected in 3..=5i64 {
        let ev = cb.expect_evt(EVT_WAIT).await;
        assert_eq!(ev["evt"], json!("channels.message"), "replay evt: {ev}");
        assert_eq!(ev["payload"]["seq"], json!(expected), "replay seq: {ev}");
    }
    // Below the cap, no overflow (and nothing else) follows.
    cb.expect_no_evt(SHORT).await;
}

// 4. A gap at the 500-row cap replays the full page then a
//    `channels.resume_overflow` telling the client to cold-load history.
//
// This originally caught a real bug: `resume_replay` bursts up to RESUME_MAX
// (500) + 1 durable frames into the 256-cap send queue, and the plain
// close-on-full push tripped the slow-consumer guard (4409) mid-catch-up. Fixed
// by `registry::push_to_awaiting` — resume now backpressures on a full queue
// instead of closing (the burst is the server's, not a slow reader's).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn resume_overflow_at_cap(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 16).await;
    let (token_a, a) = mint_full(&app, tenant, world, "a").await;
    let (token_b, b) = mint_full(&app, tenant, world, "b").await;
    let server = spawn_server(test_state(app.clone(), test_config()).await).await;

    let mut ca = connect_and_auth(server.addr, &token_a).await;
    let number_b = b.character.number.clone().expect("B number");
    let channel_id = cid(&open_direct(&mut ca, &number_b).await);
    let channel = Uuid::parse_str(&channel_id).expect("channel_id uuid");
    seed_messages(&app, world, channel, a.identity.character_id, 500).await;

    // B resumes from 0 → exactly 500 messages then the overflow signal.
    let mut cb = connect_and_auth(server.addr, &token_b).await;
    let ack = cb.cmd(sub_resume(&channel_id, 0)).await;
    assert_eq!(ack["ok"], json!(true), "B resume sub: {ack}");

    let mut msgs = 0usize;
    loop {
        let ev = cb.expect_evt(EVT_WAIT).await;
        match ev["evt"].as_str() {
            Some("channels.message") => msgs += 1,
            Some("channels.resume_overflow") => {
                assert_eq!(
                    ev["payload"]["channel_id"],
                    json!(channel_id),
                    "overflow chan: {ev}"
                );
                break;
            }
            other => panic!("unexpected evt {other:?}: {ev}"),
        }
    }
    assert_eq!(msgs, 500, "exactly 500 messages before overflow");
}

async fn members(c: &mut TestClient, channel_id: &str) -> Value {
    c.cmd(json!({ "cmd": "channels.members", "payload": { "channel_id": channel_id } }))
        .await
}

async fn set_muted(c: &mut TestClient, channel_id: &str, muted: bool) -> Value {
    c.cmd(json!({ "cmd": "channels.set_muted", "payload": {
        "channel_id": channel_id,
        "muted": muted,
    } }))
    .await
}

async fn list(c: &mut TestClient) -> Value {
    c.cmd(json!({ "cmd": "channels.list" })).await
}

/// Is `channel_id` present in a `channels.list` ack with `muted = true`.
fn muted_in_list(ack: &Value, channel_id: &str) -> bool {
    ack["payload"]
        .as_array()
        .expect("list array")
        .iter()
        .find(|s| s["channel_id"] == json!(channel_id))
        .map(|s| s["muted"] == json!(true))
        .unwrap_or(false)
}

// 5. `channels.members` returns the roster (character_id + joined_at) to a
//    member and is forbidden to a non-member (gap #3).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn members_roster_and_authz(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 16).await;
    let (token_a, _a) = mint_full(&app, tenant, world, "a").await;
    let (_token_b, b) = mint_full(&app, tenant, world, "b").await;
    let (token_e, _e) = mint_full(&app, tenant, world, "e").await;
    let server = spawn_server(test_state(app, test_config()).await).await;
    let char_b = b.identity.character_id;

    let mut ca = connect_and_auth(server.addr, &token_a).await;
    let ack = ca
        .cmd(json!({ "cmd": "channels.create", "payload": {
            "name": "g",
            "members": [char_b],
        } }))
        .await;
    let group = cid(&ack);

    // A member reads the roster: both A and B, each carrying a joined_at.
    let ack = members(&mut ca, &group).await;
    assert_eq!(ack["ok"], json!(true), "members: {ack}");
    let roster = ack["payload"].as_array().expect("roster array");
    assert_eq!(roster.len(), 2, "two members: {ack}");
    let ids: Vec<&str> = roster
        .iter()
        .map(|m| m["character_id"].as_str().expect("character_id"))
        .collect();
    assert!(
        ids.contains(&char_b.to_string().as_str()),
        "B in roster: {ack}"
    );
    assert!(
        roster.iter().all(|m| m["joined_at"].is_string()),
        "joined_at present: {ack}"
    );

    // A non-member cannot read the roster → forbidden (no existence leak).
    let mut ce = connect_and_auth(server.addr, &token_e).await;
    let ack = members(&mut ce, &group).await;
    assert_eq!(ack["err"]["code"], json!("forbidden"), "E members: {ack}");
}

// 6. `channels.set_muted` toggles the caller's own mute flag (reflected in
//    channels.list), is idempotent, and is forbidden to a non-member (gap #3).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn set_muted_toggles_and_authz(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 16).await;
    let (token_a, _a) = mint_full(&app, tenant, world, "a").await;
    let (_token_b, b) = mint_full(&app, tenant, world, "b").await;
    let (token_e, _e) = mint_full(&app, tenant, world, "e").await;
    let server = spawn_server(test_state(app, test_config()).await).await;

    let mut ca = connect_and_auth(server.addr, &token_a).await;
    let ack = ca
        .cmd(json!({ "cmd": "channels.create", "payload": {
            "name": "g",
            "members": [b.identity.character_id],
        } }))
        .await;
    let group = cid(&ack);

    // Mute → the caller's list reflects it; re-muting is idempotent; unmute clears.
    assert_eq!(set_muted(&mut ca, &group, true).await["ok"], json!(true), "mute");
    assert!(muted_in_list(&list(&mut ca).await, &group), "muted after true");
    assert_eq!(
        set_muted(&mut ca, &group, true).await["ok"],
        json!(true),
        "mute idempotent"
    );
    assert_eq!(
        set_muted(&mut ca, &group, false).await["ok"],
        json!(true),
        "unmute"
    );
    assert!(
        !muted_in_list(&list(&mut ca).await, &group),
        "unmuted after false"
    );

    // A non-member cannot mute the channel → forbidden.
    let mut ce = connect_and_auth(server.addr, &token_e).await;
    assert_eq!(
        set_muted(&mut ce, &group, true).await["err"]["code"],
        json!("forbidden"),
        "E mute"
    );
}
