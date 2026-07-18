//! Sprint 2 gateway lifecycle tests (roadmap §2 test plan): auth handshake,
//! close-code contract (§4.1/§4.3), sub authorization (§4.4), presence
//! transitions (§4.2/CDR-6), and per-character rate limiting (§12). Every test
//! drives the real router over a live socket via `common::ws`.

mod common;

use std::time::Duration;

use common::ws::{connect, connect_and_auth, mint_token, spawn_server};
use common::{app_pool, seed_world_tenant, test_config, test_state};
use opn_core::infra::ids::new_id;
use serde_json::json;
use sqlx::PgPool;

const SHORT: Duration = Duration::from_millis(500);
const CLOSE_WAIT: Duration = Duration::from_secs(5);
const EVT_WAIT: Duration = Duration::from_secs(2);

fn sub(topic: String) -> serde_json::Value {
    json!({ "cmd": "sub", "payload": { "topic": topic } })
}

// 1. Authenticated handshake, then a real command round-trips.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn auth_happy_path(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let (token, _) = mint_token(&app, tenant, world, "ref").await;
    let server = spawn_server(test_state(app, test_config()).await).await;

    let mut c = connect_and_auth(server.addr, &token).await;
    let ack = c.cmd(json!({ "cmd": "identity.me" })).await;
    assert_eq!(ack["ok"], json!(true), "identity.me not ok: {ack}");
    assert!(
        ack["payload"]["character"]["id"].is_string(),
        "no character.id in me payload: {ack}"
    );
}

// 2. No auth frame within the 3 s deadline → gateway closes 4401.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn no_auth_within_3s_closes_4401(admin: PgPool) {
    let app = app_pool(&admin, 4).await;
    let server = spawn_server(test_state(app, test_config()).await).await;

    let mut c = connect(server.addr).await;
    assert_eq!(c.expect_close(CLOSE_WAIT).await, 4401);
}

// 3. A malformed first frame — or a well-formed non-auth one — closes 4400.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn garbage_first_frame_closes_4400(admin: PgPool) {
    let app = app_pool(&admin, 4).await;
    let server = spawn_server(test_state(app, test_config()).await).await;

    let mut c = connect(server.addr).await;
    c.send_raw("not json").await;
    assert_eq!(
        c.expect_close(CLOSE_WAIT).await,
        4400,
        "garbage first frame"
    );

    // Fresh connection: valid frame, but the first cmd is not `auth`.
    let mut c = connect(server.addr).await;
    c.send_raw(r#"{"id":1,"cmd":"sub","payload":{"topic":"x"}}"#)
        .await;
    assert_eq!(
        c.expect_close(CLOSE_WAIT).await,
        4400,
        "non-auth first frame"
    );
}

// 4. Auth frame with an unverifiable token → 4401.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn bad_jwt_closes_4401(admin: PgPool) {
    let app = app_pool(&admin, 4).await;
    let server = spawn_server(test_state(app, test_config()).await).await;

    let mut c = connect_raw_auth(server.addr, "garbage").await;
    assert_eq!(c.expect_close(CLOSE_WAIT).await, 4401);
}

// Connect + send an auth frame without asserting the ack (auth is expected to fail).
async fn connect_raw_auth(addr: std::net::SocketAddr, token: &str) -> common::ws::TestClient {
    let mut c = connect(addr).await;
    c.send_raw(&common::ws::auth_frame(1, token)).await;
    c
}

// 5. Two connections on one session: the newcomer wins, the old one gets 4408.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn takeover_4408(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let (token, _) = mint_token(&app, tenant, world, "ref").await;
    let server = spawn_server(test_state(app, test_config()).await).await;

    let mut a = connect_and_auth(server.addr, &token).await;
    let mut b = connect_and_auth(server.addr, &token).await;

    assert_eq!(a.expect_close(CLOSE_WAIT).await, 4408, "A taken over");
    let ack = b.cmd(json!({ "cmd": "identity.me" })).await;
    assert_eq!(
        ack["ok"],
        json!(true),
        "B still works after takeover: {ack}"
    );
}

// 6. Two missed pongs (heartbeat 1 s) → server closes 1001.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn missed_pongs_close(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let (token, _) = mint_token(&app, tenant, world, "ref").await;
    let mut cfg = test_config();
    cfg.heartbeat_secs = 1;
    let server = spawn_server(test_state(app, cfg).await).await;

    let mut c = connect_and_auth(server.addr, &token).await;
    // Not reading = tungstenite never auto-pongs the server's pings.
    tokio::time::sleep(Duration::from_secs_f64(3.5)).await;
    assert_eq!(c.expect_close(CLOSE_WAIT).await, 1001);
}

// 7. A bad mid-connection frame acks `invalid` (id salvaged as 0) and the
//    connection stays alive.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn bad_json_mid_connection_acks_invalid_never_closes(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let (token, _) = mint_token(&app, tenant, world, "ref").await;
    let server = spawn_server(test_state(app, test_config()).await).await;

    let mut c = connect_and_auth(server.addr, &token).await;
    c.send_raw("}{ not json").await;
    let ack = c.recv_ack(CLOSE_WAIT).await;
    assert_eq!(ack["reply_to"], json!(0), "salvaged id: {ack}");
    assert_eq!(ack["ok"], json!(false));
    assert_eq!(ack["err"]["code"], json!("invalid"), "{ack}");

    let ack = c.cmd(json!({ "cmd": "identity.me" })).await;
    assert_eq!(ack["ok"], json!(true), "connection still alive: {ack}");
}

// 8. Sub authorization + one-auth-per-connection (§4.4, §4.1).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn sub_authz(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let (token, id) = mint_token(&app, tenant, world, "ref").await;
    let server = spawn_server(test_state(app, test_config()).await).await;

    let mut c = connect_and_auth(server.addr, &token).await;

    let ack = c.cmd(sub(format!("notify:{}", id.device_id))).await;
    assert_eq!(ack["ok"], json!(true), "own notify: {ack}");

    let ack = c.cmd(sub(format!("notify:{}", new_id()))).await;
    assert_eq!(
        ack["err"]["code"],
        json!("forbidden"),
        "other notify: {ack}"
    );

    let ack = c.cmd(sub(format!("ch:{}", new_id()))).await;
    assert_eq!(ack["err"]["code"], json!("not_found"), "unowned ch: {ack}");

    let ack = c.cmd(sub("garbage".into())).await;
    assert_eq!(ack["err"]["code"], json!("invalid"), "bad topic: {ack}");

    let ack = c
        .cmd(json!({ "cmd": "auth", "payload": { "token": token } }))
        .await;
    assert_eq!(ack["err"]["code"], json!("conflict"), "re-auth: {ack}");
}

// 9. Presence snapshot-before-ack, then online:true / offline transitions.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn presence_snapshot_and_transitions(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 12).await;
    let (token_a, _) = mint_token(&app, tenant, world, "a").await;
    let (token_b, id_b) = mint_token(&app, tenant, world, "b").await;
    let char_b = id_b.character_id;
    let server = spawn_server(test_state(app, test_config()).await).await;

    let mut a = connect_and_auth(server.addr, &token_a).await;
    // Sub before B exists: the snapshot Push is buffered by cmd() (it arrives
    // before the sub ack, §4.4).
    let ack = a.cmd(sub(format!("presence:{char_b}"))).await;
    assert_eq!(ack["ok"], json!(true), "sub ack: {ack}");
    let snap = a.expect_evt(SHORT).await;
    assert_eq!(snap["payload"]["online"], json!(false), "snapshot: {snap}");

    // B connects → A sees online:true.
    let b = connect_and_auth(server.addr, &token_b).await;
    let ev = a.expect_evt(EVT_WAIT).await;
    assert_eq!(ev["payload"]["online"], json!(true), "connect evt: {ev}");

    // B disconnects → A sees online:false with a last_seen_at.
    drop(b);
    let ev = a.expect_evt(EVT_WAIT).await;
    assert_eq!(
        ev["payload"]["online"],
        json!(false),
        "disconnect evt: {ev}"
    );
    assert!(
        ev["payload"]["last_seen_at"].is_string(),
        "last_seen_at set on offline transition: {ev}"
    );
}

// 10. share_presence off → snapshot online:null and zero transitions.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn share_presence_off_null_snapshot(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 12).await;
    let (token_a, _) = mint_token(&app, tenant, world, "a").await;
    let (token_b, id_b) = mint_token(&app, tenant, world, "b").await;
    let char_b = id_b.character_id;
    // Owner (admin) bypasses RLS; flip B's sharing off directly.
    sqlx::query("UPDATE characters SET share_presence = false WHERE id = $1")
        .bind(char_b)
        .execute(&admin)
        .await
        .expect("disable share_presence");
    let server = spawn_server(test_state(app, test_config()).await).await;

    let mut a = connect_and_auth(server.addr, &token_a).await;
    let ack = a.cmd(sub(format!("presence:{char_b}"))).await;
    assert_eq!(ack["ok"], json!(true), "sub ack: {ack}");
    let snap = a.expect_evt(SHORT).await;
    assert_eq!(
        snap["payload"]["online"],
        json!(null),
        "null snapshot: {snap}"
    );

    // B connects and leaves — a non-sharing character emits nothing.
    let b = connect_and_auth(server.addr, &token_b).await;
    drop(b);
    a.expect_no_evt(Duration::from_millis(1500)).await;
}

// 11. Social class = 5/s burst 20: the 21st rapid command is rate limited.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn rate_limited_ack(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let (token, _) = mint_token(&app, tenant, world, "ref").await;
    let server = spawn_server(test_state(app, test_config()).await).await;

    let mut c = connect_and_auth(server.addr, &token).await;
    let cmd = json!({ "cmd": "identity.set_share_presence", "payload": { "on": true } });
    for i in 0..20 {
        let ack = c.cmd(cmd.clone()).await;
        assert_eq!(ack["ok"], json!(true), "burst cmd {i}: {ack}");
    }
    let ack = c.cmd(cmd).await;
    assert_eq!(ack["err"]["code"], json!("rate_limited"), "21st: {ack}");
    assert!(
        ack["payload"]["retry_after_ms"].as_u64().unwrap_or(0) > 0,
        "retry hint: {ack}"
    );
}

// 12. auth.refresh over the live socket: fresh usable JWT, revoked session
// refused (§11).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn auth_refresh_returns_fresh_token(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let (token, identity) = mint_token(&app, tenant, world, "ref").await;
    let server = spawn_server(test_state(app.clone(), test_config()).await).await;

    let mut c = connect_and_auth(server.addr, &token).await;
    let ack = c.cmd(json!({ "cmd": "auth.refresh" })).await;
    assert_eq!(ack["ok"], json!(true), "refresh: {ack}");
    let fresh = ack["payload"]["token"].as_str().expect("token in payload");
    assert!(!fresh.is_empty());
    // The fresh token authenticates a new connection (same session).
    let mut c2 = connect_and_auth(server.addr, fresh).await;
    let ack = c2.cmd(json!({ "cmd": "identity.me" })).await;
    assert_eq!(ack["ok"], json!(true), "fresh token works: {ack}");

    // Revoke underneath the live connection → refresh refuses.
    sqlx::query("UPDATE sessions SET revoked_at = now() WHERE id = $1")
        .bind(identity.session_id)
        .execute(&admin)
        .await
        .expect("revoke");
    let ack = c2.cmd(json!({ "cmd": "auth.refresh" })).await;
    assert_eq!(ack["err"]["code"], json!("unauthorized"), "revoked: {ack}");
}
