//! Sprint 6 part B tenant-link tests (OPN-CORE.md §5): the down-only
//! voice-target channel. Drives real `/link` WebSockets (API-key authed)
//! alongside client call sockets and asserts `calls.voice` set_targets/clear,
//! last-writer takeover, the disconnected-tenant drop, the `/calls/active`
//! re-sync, the hello handshake, and the janitor reap's `clear`.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use common::ws::{
    connect_and_auth, connect_link, connect_link_hello, mint_full, spawn_server, TestClient,
    TestServer,
};
use common::{app_pool, seed_world_tenant, test_config, test_state};
use opn_core::infra::db::world_tx;
use opn_core::primitives::calls;
use opn_core::state::AppState;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

const EVT_WAIT: Duration = Duration::from_secs(2);

// ── command builders ─────────────────────────────────────────────────────────

async fn calls_start(c: &mut TestClient, number: &str, video: bool) -> Value {
    c.cmd(json!({ "cmd": "calls.start", "payload": { "callee_number": number, "video": video } }))
        .await
}
async fn calls_accept(c: &mut TestClient, call_id: &str) -> Value {
    c.cmd(json!({ "cmd": "calls.accept", "payload": { "call_id": call_id } }))
        .await
}
async fn calls_hangup(c: &mut TestClient, call_id: &str) -> Value {
    c.cmd(json!({ "cmd": "calls.hangup", "payload": { "call_id": call_id } }))
        .await
}
async fn calls_decline(c: &mut TestClient, call_id: &str) -> Value {
    c.cmd(json!({ "cmd": "calls.decline", "payload": { "call_id": call_id } }))
        .await
}
fn call_id_of(ack: &Value) -> String {
    ack["payload"]["call_id"]
        .as_str()
        .unwrap_or_else(|| panic!("no call_id: {ack}"))
        .to_string()
}

/// The next link push must be a `calls.voice`; returns `(action, sorted chars)`.
async fn expect_voice(link: &mut TestClient) -> (String, Vec<String>) {
    let ev = link.expect_evt(EVT_WAIT).await;
    assert_eq!(
        ev["evt"],
        json!("calls.voice"),
        "expected calls.voice: {ev}"
    );
    let action = ev["payload"]["action"]
        .as_str()
        .unwrap_or_else(|| panic!("no action: {ev}"))
        .to_string();
    let mut chars: Vec<String> = ev["payload"]["characters"]
        .as_array()
        .unwrap_or_else(|| panic!("no characters: {ev}"))
        .iter()
        .map(|c| c.as_str().expect("char string").to_string())
        .collect();
    chars.sort();
    (action, chars)
}

// ── shared setup ─────────────────────────────────────────────────────────────

struct Setup {
    server: TestServer,
    addr: SocketAddr,
    state: AppState,
    world: Uuid,
    key: String,
    caller: TestClient,
    callee: TestClient,
    caller_char: Uuid,
    callee_char: Uuid,
    callee_num: String,
}

/// A live server, a connected+authed caller and callee (each with a number),
/// and the tenant API key for the link — everything a link test drives.
async fn setup(admin: &PgPool) -> Setup {
    let (world, tenant, key) = seed_world_tenant(admin).await;
    let pool = app_pool(admin, 8).await;
    let state = test_state(pool, test_config()).await;
    let server = spawn_server(state.clone()).await;
    let addr = server.addr;

    let (a_tok, a) = mint_full(&state.pg, tenant, world, "caller").await;
    let (b_tok, b) = mint_full(&state.pg, tenant, world, "callee").await;
    let caller = connect_and_auth(addr, &a_tok).await;
    let callee = connect_and_auth(addr, &b_tok).await;
    Setup {
        server,
        addr,
        state,
        world,
        key,
        caller,
        callee,
        caller_char: a.identity.character_id,
        callee_char: b.identity.character_id,
        callee_num: b.character.number.expect("callee number"),
    }
}

fn sorted(mut v: Vec<Uuid>) -> Vec<String> {
    let mut s: Vec<String> = v.drain(..).map(|u| u.to_string()).collect();
    s.sort();
    s
}

/// Poll until every character has no live connection (the server has processed
/// the socket drop) — the orphan reaper's liveness signal.
async fn wait_offline(state: &AppState, world: Uuid, chars: &[Uuid]) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if chars
            .iter()
            .all(|c| !state.registry.is_character_online(world, *c))
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "clients did not go offline in time"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

/// The exit-criteria demo: a call connects, the link receives `set_targets`, and
/// hangups clear the targets — including the intermediate state as one party
/// leaves an active call while the other is still joined.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn set_targets_on_accept_and_clear_on_hangup(admin: PgPool) {
    let mut s = setup(&admin).await;
    let mut link = connect_link_hello(s.addr, &s.key).await;

    let ack = calls_start(&mut s.caller, &s.callee_num, true).await;
    let call_id = call_id_of(&ack);
    // Ringing → no voice targets yet (a ring emits nothing on the link).
    calls_accept(&mut s.callee, &call_id).await; // → active, both joined

    let (action, chars) = expect_voice(&mut link).await;
    assert_eq!(action, "set_targets");
    assert_eq!(
        chars,
        sorted(vec![s.caller_char, s.callee_char]),
        "both joined parties are voice targets"
    );

    // Caller hangs up; callee is still joined → active continues → targets shrink.
    calls_hangup(&mut s.caller, &call_id).await;
    let (action, chars) = expect_voice(&mut link).await;
    assert_eq!(action, "set_targets");
    assert_eq!(
        chars,
        sorted(vec![s.callee_char]),
        "only callee left joined"
    );

    // Callee hangs up → last joined leaves → session ends → clear.
    calls_hangup(&mut s.callee, &call_id).await;
    let (action, chars) = expect_voice(&mut link).await;
    assert_eq!(action, "clear");
    assert!(chars.is_empty(), "clear carries no targets: {chars:?}");

    drop(s.server);
}

/// One link per world, last-writer-wins: a second link takes over and the first
/// is closed 4408; the survivor still receives voice events.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn second_link_takes_over_first(admin: PgPool) {
    let mut s = setup(&admin).await;
    let mut link1 = connect_link_hello(s.addr, &s.key).await;
    let mut link2 = connect_link_hello(s.addr, &s.key).await;

    assert_eq!(
        link1.expect_close(EVT_WAIT).await,
        4408,
        "the superseded link is closed TAKEN_OVER"
    );

    // The survivor is the live link: an accept reaches link2.
    let ack = calls_start(&mut s.caller, &s.callee_num, false).await;
    let call_id = call_id_of(&ack);
    calls_accept(&mut s.callee, &call_id).await;
    let (action, _chars) = expect_voice(&mut link2).await;
    assert_eq!(action, "set_targets", "successor link receives voice");

    drop(s.server);
}

/// Link down = events dropped by design (§5): a call still connects and the
/// handlers succeed with no link connected — the voice emit is a silent no-op.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn no_link_connected_call_still_works(admin: PgPool) {
    let mut s = setup(&admin).await;
    let ack = calls_start(&mut s.caller, &s.callee_num, false).await;
    let call_id = call_id_of(&ack);
    let ack = calls_accept(&mut s.callee, &call_id).await;
    assert_eq!(
        ack["ok"],
        json!(true),
        "accept succeeds with no tenant link: {ack}"
    );
    drop(s.server);
}

/// `GET /v1/tenants/self/calls/active` re-sync (§5): reflects live call state —
/// the active call with both participants, then empty once both hang up.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn active_calls_resync_reflects_state(admin: PgPool) {
    let mut s = setup(&admin).await;
    let ack = calls_start(&mut s.caller, &s.callee_num, true).await;
    let call_id = call_id_of(&ack);
    calls_accept(&mut s.callee, &call_id).await;

    let body = get_active(s.addr, &s.key).await;
    let arr = body.as_array().expect("active is an array");
    assert_eq!(arr.len(), 1, "one active call: {body}");
    assert_eq!(arr[0]["call_id"], json!(call_id));
    assert_eq!(arr[0]["state"], json!("active"));
    assert_eq!(arr[0]["kind"], json!("video"));
    assert_eq!(
        arr[0]["participants"]
            .as_array()
            .expect("participants")
            .len(),
        2,
        "both participants present for re-sync: {body}"
    );

    // End the call → the re-sync no longer lists it.
    calls_hangup(&mut s.caller, &call_id).await;
    calls_hangup(&mut s.callee, &call_id).await;
    let body = get_active(s.addr, &s.key).await;
    assert_eq!(
        body.as_array().expect("array").len(),
        0,
        "ended call drops out of the re-sync: {body}"
    );

    drop(s.server);
}

async fn get_active(addr: SocketAddr, key: &str) -> Value {
    let resp = reqwest::Client::new()
        .get(format!("http://{addr}/v1/tenants/self/calls/active"))
        .header("authorization", format!("Bearer {key}"))
        .send()
        .await
        .expect("GET /calls/active");
    assert_eq!(resp.status(), 200, "active status");
    let text = resp.text().await.expect("active body");
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("active json ({e}): {text}"))
}

/// The janitor reap ends an un-accepted ring AND clears its voice targets (§5):
/// a stale ring past 60 s is force-ended and the link receives `clear`. Runs the
/// janitor's own body (reap → publish_snapshot) so the emit path is exercised.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn reap_emits_clear(admin: PgPool) {
    let mut s = setup(&admin).await;
    let mut link = connect_link_hello(s.addr, &s.key).await;

    let ack = calls_start(&mut s.caller, &s.callee_num, false).await;
    let call_id = Uuid::parse_str(&call_id_of(&ack)).expect("call_id uuid");

    // Age the ring past the 60 s reap window (RLS-scoped, like production).
    {
        let mut tx = world_tx(&s.state.pg, s.world).await.expect("tx");
        sqlx::query(
            "UPDATE call_sessions SET created_at = now() - interval '2 minutes' WHERE id = $1",
        )
        .bind(call_id)
        .execute(&mut *tx)
        .await
        .expect("age the ring");
        tx.commit().await.expect("commit");
    }

    // The janitor body: reap the zombie, publish the final snapshot (which also
    // emits the tenant-link clear).
    let snaps = calls::store::reap_zombie_rings(&s.state.pg, s.world)
        .await
        .expect("reap");
    assert_eq!(snaps.len(), 1, "the stale ring is reaped");
    for snap in &snaps {
        calls::publish_snapshot(&s.state, s.world, snap).await;
    }

    let (action, chars) = expect_voice(&mut link).await;
    assert_eq!(action, "clear", "a reaped ring clears voice targets");
    assert!(chars.is_empty());

    drop(s.server);
}

/// The keeper (adversarial-review MED): an ACTIVE call both parties drop without
/// hanging up must not strand voice targets. The orphan reaper ends it and the
/// link receives `clear`. Runs the janitor's own body (candidates → offline
/// filter → end_active_orphans → publish_snapshot).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn orphaned_active_call_reaped_emits_clear(admin: PgPool) {
    let mut s = setup(&admin).await;
    let mut link = connect_link_hello(s.addr, &s.key).await;

    let ack = calls_start(&mut s.caller, &s.callee_num, true).await;
    let call_id = call_id_of(&ack);
    let call_uuid = Uuid::parse_str(&call_id).expect("call_id uuid");
    calls_accept(&mut s.callee, &call_id).await; // → active, both joined
    let (action, _) = expect_voice(&mut link).await;
    assert_eq!(action, "set_targets", "active call bound voice");

    // Both clients crash — sockets drop with no calls.hangup. No FSM transition
    // fires (a WS disconnect never transitions a participant row), so the call
    // is stranded 'active'.
    let (caller_char, callee_char) = (s.caller_char, s.callee_char);
    drop(s.caller);
    drop(s.callee);
    wait_offline(&s.state, s.world, &[caller_char, callee_char]).await;

    // Age past the 60 s active-orphan window.
    {
        let mut tx = world_tx(&s.state.pg, s.world).await.expect("tx");
        sqlx::query(
            "UPDATE call_sessions SET created_at = now() - interval '2 minutes' WHERE id = $1",
        )
        .bind(call_uuid)
        .execute(&mut *tx)
        .await
        .expect("age the call");
        tx.commit().await.expect("commit");
    }

    // The janitor body: pick offline-orphan candidates, end them, emit.
    let candidates = calls::store::active_reap_candidates(&s.state.pg, s.world)
        .await
        .expect("candidates");
    let dead: Vec<Uuid> = candidates
        .into_iter()
        .filter(|(_, chars)| {
            chars
                .iter()
                .all(|c| !s.state.registry.is_character_online(s.world, *c))
        })
        .map(|(id, _)| id)
        .collect();
    assert_eq!(
        dead,
        vec![call_uuid],
        "the orphaned active call is a candidate"
    );
    let snaps = calls::store::end_active_orphans(&s.state.pg, s.world, &dead)
        .await
        .expect("end orphans");
    assert_eq!(snaps.len(), 1, "one orphan ended");
    for snap in &snaps {
        calls::publish_snapshot(&s.state, s.world, snap).await;
    }

    let (action, chars) = expect_voice(&mut link).await;
    assert_eq!(action, "clear", "orphaned active call clears voice");
    assert!(chars.is_empty());

    // Idempotent: a live call is spared. (A second sweep finds nothing — the
    // call is now 'ended', excluded by the `state = 'active'` guard.)
    let again = calls::store::active_reap_candidates(&s.state.pg, s.world)
        .await
        .expect("candidates again");
    assert!(again.is_empty(), "ended call is not a re-reap candidate");

    drop(s.server);
}

/// Declining a ring emits NO voice event (§5): a ring never bound targets, so
/// there is nothing to set or clear. Covers `emit_voice`'s Ringing branch.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn decline_emits_no_voice(admin: PgPool) {
    let mut s = setup(&admin).await;
    let mut link = connect_link_hello(s.addr, &s.key).await;

    let ack = calls_start(&mut s.caller, &s.callee_num, false).await;
    let call_id = call_id_of(&ack);
    // Callee declines; the caller is still joined, so the session stays ringing
    // (not ended) — no voice was ever set, so the link stays silent.
    calls_decline(&mut s.callee, &call_id).await;
    link.expect_no_evt(Duration::from_millis(400)).await;

    drop(s.server);
}

/// A malformed first frame (not a `hello`) closes the link 4400 — the resource
/// must handshake before anything else.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn bad_hello_closes_4400(admin: PgPool) {
    let s = setup(&admin).await;
    let mut link = connect_link(s.addr, &s.key).await.expect("link connect");
    link.send_raw(&json!({ "not": "a hello" }).to_string())
        .await;
    assert_eq!(link.expect_close(EVT_WAIT).await, 4400);
    drop(s.server);
}

/// The link is API-key gated: a bogus key is rejected at the upgrade (401), so
/// the WebSocket never opens.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn bad_api_key_rejected(admin: PgPool) {
    let s = setup(&admin).await;
    let res = connect_link(s.addr, "opn_definitely_not_a_key").await;
    assert!(res.is_err(), "bad api key must be rejected pre-upgrade");
    drop(s.server);
}
