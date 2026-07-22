//! servers primitive integration tests (OPN-CORE.md §10.2a, contract gap
//! #13): container CRUD, the channel-membership mirror, and its authz edges.
//! Server channels are ordinary channels — send/list plumbing is exercised
//! through the same WS surface as channels.rs.

mod common;

use common::ws::{connect_and_auth, mint_full, spawn_server, TestClient};
use common::{app_pool, seed_world_tenant, test_config, test_state};
use opn_core::infra::ids::new_id;
use serde_json::{json, Value};
use sqlx::PgPool;

async fn servers_create(c: &mut TestClient, name: &str) -> Value {
    c.cmd(json!({ "cmd": "servers.create", "payload": {
        "name": name, "banner_media_id": null,
    } }))
    .await
}

async fn servers_list(c: &mut TestClient) -> Value {
    c.cmd(json!({ "cmd": "servers.list" })).await
}

async fn channel_create(
    c: &mut TestClient,
    server_id: &str,
    name: &str,
    kind: &str,
    category: Option<&str>,
    position: i32,
) -> Value {
    c.cmd(json!({ "cmd": "servers.channel_create", "payload": {
        "server_id": server_id, "name": name, "kind": kind,
        "category": category, "position": position,
    } }))
    .await
}

async fn member_change(c: &mut TestClient, cmd: &str, server_id: &str, character: &str) -> Value {
    c.cmd(json!({ "cmd": cmd, "payload": {
        "server_id": server_id, "character_id": character,
    } }))
    .await
}

async fn channels_list(c: &mut TestClient) -> Value {
    c.cmd(json!({ "cmd": "channels.list" })).await
}

/// The summary for `channel_id` in a `channels.list` ack payload, if present.
fn channel_in_list<'a>(ack: &'a Value, channel_id: &str) -> Option<&'a Value> {
    ack["payload"]
        .as_array()?
        .iter()
        .find(|c| c["channel_id"] == json!(channel_id))
}

// 1. Create + tree fields: a server with a categorized text channel and a
//    voice channel; servers.list carries owner/count, channels.list carries
//    server_id/category/position; bad kind and non-owner create rejected.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn create_server_channels_and_list(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 16).await;
    let (token_a, a) = mint_full(&app, tenant, world, "a").await;
    let (token_b, _b) = mint_full(&app, tenant, world, "b").await;
    let server = spawn_server(test_state(app, test_config()).await).await;

    let mut ca = connect_and_auth(server.addr, &token_a).await;
    let ack = servers_create(&mut ca, "The Lounge").await;
    assert_eq!(ack["ok"], json!(true), "create: {ack}");
    let server_id = ack["payload"]["server_id"].as_str().expect("server_id").to_string();

    let ack = channel_create(&mut ca, &server_id, "general", "group", Some("Text"), 0).await;
    assert_eq!(ack["ok"], json!(true), "text channel: {ack}");
    let text_id = ack["payload"]["channel_id"].as_str().expect("channel_id").to_string();
    let ack = channel_create(&mut ca, &server_id, "Voice Lounge", "voice", None, 1).await;
    assert_eq!(ack["ok"], json!(true), "voice channel: {ack}");
    let voice_id = ack["payload"]["channel_id"].as_str().expect("channel_id").to_string();

    // Kind is group|voice only; sms and friends stay phone-plane kinds.
    let ack = channel_create(&mut ca, &server_id, "nope", "sms", None, 0).await;
    assert_eq!(ack["err"]["code"], json!("invalid"), "bad kind: {ack}");

    let ack = servers_list(&mut ca).await;
    assert_eq!(ack["ok"], json!(true), "list: {ack}");
    let rail = ack["payload"].as_array().expect("array");
    assert_eq!(rail.len(), 1, "one server: {ack}");
    assert_eq!(rail[0]["name"], json!("The Lounge"), "{ack}");
    assert_eq!(
        rail[0]["owner_character_id"],
        json!(a.identity.character_id),
        "{ack}"
    );
    assert_eq!(rail[0]["member_count"], json!(1), "{ack}");
    assert_eq!(rail[0]["banner_media_id"], json!(null), "{ack}");

    let ack = channels_list(&mut ca).await;
    let text = channel_in_list(&ack, &text_id).expect("text row");
    assert_eq!(text["server_id"], json!(server_id), "{ack}");
    assert_eq!(text["category"], json!("Text"), "{ack}");
    assert_eq!(text["position"], json!(0), "{ack}");
    assert_eq!(text["kind"], json!("group"), "{ack}");
    let voice = channel_in_list(&ack, &voice_id).expect("voice row");
    assert_eq!(voice["kind"], json!("voice"), "{ack}");
    assert_eq!(voice["category"], json!(null), "{ack}");
    assert_eq!(voice["position"], json!(1), "{ack}");

    // Non-owner (non-member, even) can't create channels or see the server.
    let mut cb = connect_and_auth(server.addr, &token_b).await;
    let ack = channel_create(&mut cb, &server_id, "sneaky", "group", None, 0).await;
    assert_eq!(ack["err"]["code"], json!("forbidden"), "non-owner: {ack}");
    let ack = servers_list(&mut cb).await;
    assert_eq!(ack["payload"], json!([]), "b sees nothing: {ack}");

    // A dangling banner id is invalid at create.
    let ack = ca
        .cmd(json!({ "cmd": "servers.create", "payload": {
            "name": "x", "banner_media_id": new_id(),
        } }))
        .await;
    assert_eq!(ack["err"]["code"], json!("invalid"), "dead banner: {ack}");
}

// 2. Membership mirror + authz: add mirrors into existing channels (and
//    later-created channels include existing members); remove un-mirrors and
//    blocks sends; owner-only add, self-leave, owner-can't-leave; direct
//    channels.member_add on a server channel conflicts.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn membership_mirror_and_authz(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 16).await;
    let (token_a, a) = mint_full(&app, tenant, world, "a").await;
    let (token_b, b) = mint_full(&app, tenant, world, "b").await;
    let server = spawn_server(test_state(app, test_config()).await).await;
    let a_id = a.identity.character_id.to_string();
    let b_id = b.identity.character_id.to_string();

    let mut ca = connect_and_auth(server.addr, &token_a).await;
    let mut cb = connect_and_auth(server.addr, &token_b).await;
    let ack = servers_create(&mut ca, "Guild").await;
    let server_id = ack["payload"]["server_id"].as_str().expect("server_id").to_string();
    let ack = channel_create(&mut ca, &server_id, "general", "group", None, 0).await;
    let general = ack["payload"]["channel_id"].as_str().expect("channel_id").to_string();

    // Non-owner can't add; unknown character is invalid.
    let ack = member_change(&mut cb, "servers.member_add", &server_id, &b_id).await;
    assert_eq!(ack["err"]["code"], json!("forbidden"), "owner-only add: {ack}");
    let ack = member_change(&mut ca, "servers.member_add", &server_id, &new_id().to_string()).await;
    assert_eq!(ack["err"]["code"], json!("invalid"), "ghost member: {ack}");

    // Owner adds B: B now sees the server AND its pre-existing channel.
    let ack = member_change(&mut ca, "servers.member_add", &server_id, &b_id).await;
    assert_eq!(ack["ok"], json!(true), "add b: {ack}");
    let ack = servers_list(&mut cb).await;
    assert_eq!(ack["payload"][0]["member_count"], json!(2), "count: {ack}");
    let ack = channels_list(&mut cb).await;
    assert!(channel_in_list(&ack, &general).is_some(), "mirrored: {ack}");

    // A channel created after the add includes B too.
    let ack = channel_create(&mut ca, &server_id, "memes", "group", None, 1).await;
    let memes = ack["payload"]["channel_id"].as_str().expect("channel_id").to_string();
    let ack = channels_list(&mut cb).await;
    assert!(channel_in_list(&ack, &memes).is_some(), "roster copied: {ack}");

    // B can actually speak in a server channel (it's a plain channel).
    let ack = cb
        .cmd(json!({ "cmd": "channels.send", "payload": {
            "channel_id": general, "client_uuid": new_id(), "body": { "text": "hey" },
        } }))
        .await;
    assert_eq!(ack["ok"], json!(true), "send: {ack}");

    // Server-channel membership is servers.*-only.
    let ack = cb
        .cmd(json!({ "cmd": "channels.member_remove", "payload": {
            "channel_id": general, "character_id": b_id,
        } }))
        .await;
    assert_eq!(ack["err"]["code"], json!("conflict"), "mirror-only: {ack}");

    // Owner can't leave; B can. After leaving, the channels are gone for B.
    let ack = member_change(&mut ca, "servers.member_remove", &server_id, &a_id).await;
    assert_eq!(ack["err"]["code"], json!("conflict"), "owner stays: {ack}");
    let ack = member_change(&mut cb, "servers.member_remove", &server_id, &b_id).await;
    assert_eq!(ack["ok"], json!(true), "self-leave: {ack}");
    let ack = servers_list(&mut cb).await;
    assert_eq!(ack["payload"], json!([]), "left: {ack}");
    let ack = cb
        .cmd(json!({ "cmd": "channels.send", "payload": {
            "channel_id": general, "client_uuid": new_id(), "body": { "text": "still here?" },
        } }))
        .await;
    assert_eq!(ack["err"]["code"], json!("forbidden"), "unmirrored: {ack}");

    // Re-add + owner kick round out the remove arm (idempotent add covered:
    // second add is a no-op ack).
    let ack = member_change(&mut ca, "servers.member_add", &server_id, &b_id).await;
    assert_eq!(ack["ok"], json!(true), "re-add: {ack}");
    let ack = member_change(&mut ca, "servers.member_add", &server_id, &b_id).await;
    assert_eq!(ack["ok"], json!(true), "idempotent add: {ack}");
    let ack = member_change(&mut ca, "servers.member_remove", &server_id, &b_id).await;
    assert_eq!(ack["ok"], json!(true), "kick: {ack}");
    let ack = servers_list(&mut cb).await;
    assert_eq!(ack["payload"], json!([]), "kicked: {ack}");
}
