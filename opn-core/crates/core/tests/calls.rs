//! Sprint 6 part A calls tests (OPN-CORE.md §10.4): the voice/video session
//! FSM at the DB + protocol layer. The pure `apply` already has unit tests in
//! `fsm.rs`; here we drive the store/handler seam and the WS wire.
//!
//! Two styles, both from the shared harness:
//!   * WS protocol tests (`common::ws`) for anything with real pushes — the
//!     ring on `notify:<device>`, snapshot-on-sub, the signal relay, the
//!     `calls.state` fan-out.
//!   * Direct-primitive tests (à la `directory.rs`) for FSM branches, the
//!     janitor reap, RLS, and concurrency — where a `world_tx` DB read is a
//!     sharper assertion than draining events.

mod common;

use std::time::Duration;

use common::ws::{connect_and_auth, mint_full, spawn_server, TestClient};
use common::{app_pool, seed_world_tenant, test_config, test_state};
use contracts::{CallKind, CallParticipantState, CallSessionState, ErrCode};
use opn_core::infra::auth::Identity;
use opn_core::infra::db::world_tx;
use opn_core::primitives::{calls, identity, Fail};
use opn_core::state::AppState;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

const EVT_WAIT: Duration = Duration::from_secs(2);
const SHORT: Duration = Duration::from_millis(400);

// ── WS command builders ──────────────────────────────────────────────────

fn sub(topic: String) -> Value {
    json!({ "cmd": "sub", "payload": { "topic": topic } })
}

async fn calls_start(c: &mut TestClient, number: &str, video: bool) -> Value {
    c.cmd(json!({ "cmd": "calls.start", "payload": {
        "callee_number": number,
        "video": video,
    } }))
    .await
}

async fn calls_signal(c: &mut TestClient, call_id: &str, to: Uuid, payload: Value) -> Value {
    c.cmd(json!({ "cmd": "calls.signal", "payload": {
        "call_id": call_id,
        "to": to,
        "payload": payload,
    } }))
    .await
}

/// `call_id` off a `calls.start` ack, panicking with the ack on absence.
fn call_id_of(ack: &Value) -> String {
    ack["payload"]["call_id"]
        .as_str()
        .unwrap_or_else(|| panic!("no call_id: {ack}"))
        .to_string()
}

/// A participant's `state` string out of a `calls.state` event payload.
fn part_state(ev: &Value, character: Uuid) -> String {
    ev["payload"]["participants"]
        .as_array()
        .unwrap_or_else(|| panic!("no participants array: {ev}"))
        .iter()
        .find(|p| p["character_id"] == json!(character))
        .and_then(|p| p["state"].as_str())
        .unwrap_or_else(|| panic!("no participant {character} in: {ev}"))
        .to_string()
}

// ── direct-primitive support ─────────────────────────────────────────────

/// AppState (RLS-on `opn_app` pool, live Redis) + a caller and callee, each a
/// character with an assigned number. No server — direct handler calls.
async fn state_and_two(admin: &PgPool) -> (AppState, Identity, String, Identity, String) {
    let (world_id, tenant_id, _key) = seed_world_tenant(admin).await;
    let pool = app_pool(admin, 8).await;
    let state = test_state(pool, test_config()).await;
    let a = identity::mint_session(&state.pg, tenant_id, world_id, "caller", None, 600)
        .await
        .expect("mint caller");
    let b = identity::mint_session(&state.pg, tenant_id, world_id, "callee", None, 600)
        .await
        .expect("mint callee");
    let a_num = a.character.number.clone().expect("caller number");
    let b_num = b.character.number.clone().expect("callee number");
    (state, a.identity, a_num, b.identity, b_num)
}

fn parse_call_id(out: &Value) -> Uuid {
    out["call_id"]
        .as_str()
        .expect("call_id str")
        .parse()
        .expect("call_id uuid")
}

/// `(session_state, [(character, participant_state)])` for a call, RLS-scoped.
async fn call_row(state: &AppState, world: Uuid, call_id: Uuid) -> (String, Vec<(Uuid, String)>) {
    let mut tx = world_tx(&state.pg, world).await.expect("tx");
    let session: String = sqlx::query_scalar("SELECT state FROM call_sessions WHERE id = $1")
        .bind(call_id)
        .fetch_one(&mut *tx)
        .await
        .expect("session state");
    let parts: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT character_id, state FROM call_participants WHERE call_id = $1 ORDER BY character_id",
    )
    .bind(call_id)
    .fetch_all(&mut *tx)
    .await
    .expect("participants");
    tx.commit().await.expect("commit");
    (session, parts)
}

fn state_of(parts: &[(Uuid, String)], who: Uuid) -> &str {
    parts
        .iter()
        .find(|(c, _)| *c == who)
        .map(|(_, s)| s.as_str())
        .unwrap_or_else(|| panic!("no participant {who}"))
}

// ═══════════════════════════════════════════════════════════════════════════
// REQUIRED — the named tests the coverage match-test points at.
// ═══════════════════════════════════════════════════════════════════════════

/// 1. End-to-end over the wire: start → ring → sub both sides → accept →
///    signal relay → hangup to `ended`. Asserts the snapshot fields at every
///    step and both peers' views of the `calls.state` fan-out.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn full_lifecycle_start_accept_signal_hangup(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 16).await;
    let (token_a, a) = mint_full(&app, tenant, world, "a").await;
    let (token_b, b) = mint_full(&app, tenant, world, "b").await;
    let server = spawn_server(test_state(app, test_config()).await).await;

    let a_char = a.identity.character_id;
    let b_char = b.identity.character_id;
    let a_num = a.character.number.clone().expect("A number");
    let b_num = b.character.number.clone().expect("B number");

    let mut ca = connect_and_auth(server.addr, &token_a).await;
    let mut cb = connect_and_auth(server.addr, &token_b).await;

    // The dialer carries no standing call sub — the callee only listens on its
    // own notify topic to hear a ring (§10.4/§10.8).
    let ack = cb
        .cmd(sub(format!("notify:{}", b.identity.device_id)))
        .await;
    assert_eq!(ack["ok"], json!(true), "B notify sub: {ack}");

    // Caller places a voice call.
    let ack = calls_start(&mut ca, &b_num, false).await;
    assert_eq!(ack["ok"], json!(true), "start: {ack}");
    let call_id = call_id_of(&ack);

    // The ring arrives on the callee's notify topic, class `ring`, from `dialer`.
    let ring = cb.expect_evt(EVT_WAIT).await;
    assert_eq!(ring["evt"], json!("notify.event"), "ring evt: {ring}");
    assert_eq!(
        ring["payload"]["app_id"],
        json!("dialer"),
        "ring app: {ring}"
    );
    assert_eq!(
        ring["payload"]["kind"],
        json!("incoming_call"),
        "ring kind: {ring}"
    );
    assert_eq!(
        ring["payload"]["class"],
        json!("ring"),
        "ring class: {ring}"
    );
    assert_eq!(
        ring["payload"]["payload"]["call_id"],
        json!(call_id),
        "ring call_id: {ring}"
    );
    assert_eq!(
        ring["payload"]["payload"]["caller_number"],
        json!(a_num),
        "ring caller_number: {ring}"
    );
    assert_eq!(
        ring["payload"]["payload"]["video"],
        json!(false),
        "ring video: {ring}"
    );

    // Both parties subscribe `call:<id>` and get a snapshot-before-ack (§4.4):
    // ringing session, caller joined, callee ringing.
    let ack = ca.cmd(sub(format!("call:{call_id}"))).await;
    assert_eq!(ack["ok"], json!(true), "A call sub: {ack}");
    let snap = ca.expect_evt(EVT_WAIT).await;
    assert_eq!(snap["evt"], json!("calls.state"), "A snap evt: {snap}");
    assert_eq!(snap["payload"]["kind"], json!("voice"), "kind: {snap}");
    assert_eq!(snap["payload"]["state"], json!("ringing"), "state: {snap}");
    assert_eq!(part_state(&snap, a_char), "joined", "caller joined: {snap}");
    assert_eq!(
        part_state(&snap, b_char),
        "ringing",
        "callee ringing: {snap}"
    );

    let ack = cb.cmd(sub(format!("call:{call_id}"))).await;
    assert_eq!(ack["ok"], json!(true), "B call sub: {ack}");
    let snap = cb.expect_evt(EVT_WAIT).await;
    assert_eq!(snap["payload"]["state"], json!("ringing"), "B snap: {snap}");

    // Callee accepts → session active, callee joined. Both subscribers see it.
    let ack = cb
        .cmd(json!({ "cmd": "calls.accept", "payload": { "call_id": call_id } }))
        .await;
    assert_eq!(ack["ok"], json!(true), "accept: {ack}");
    for (label, c) in [("A", &mut ca), ("B", &mut cb)] {
        let ev = c.expect_evt(EVT_WAIT).await;
        assert_eq!(ev["evt"], json!("calls.state"), "{label} accept evt: {ev}");
        assert_eq!(
            ev["payload"]["state"],
            json!("active"),
            "{label} active: {ev}"
        );
        assert_eq!(
            part_state(&ev, b_char),
            "joined",
            "{label} callee joined: {ev}"
        );
        assert_eq!(
            part_state(&ev, a_char),
            "joined",
            "{label} caller joined: {ev}"
        );
    }

    // A relays an opaque signal to B; it forwards verbatim on `call:<id>` with
    // the correct from/to. Both subscribers receive the durable relay (clients
    // filter by `to`).
    let ack = calls_signal(&mut ca, &call_id, b_char, json!({ "sdp": "offer-1" })).await;
    assert_eq!(ack["ok"], json!(true), "signal: {ack}");
    for (label, c) in [("A", &mut ca), ("B", &mut cb)] {
        let ev = c.expect_evt(EVT_WAIT).await;
        assert_eq!(ev["evt"], json!("calls.signal"), "{label} signal evt: {ev}");
        assert_eq!(ev["payload"]["from"], json!(a_char), "{label} from: {ev}");
        assert_eq!(ev["payload"]["to"], json!(b_char), "{label} to: {ev}");
        assert_eq!(
            ev["payload"]["payload"]["sdp"],
            json!("offer-1"),
            "{label} sdp: {ev}"
        );
    }

    // Caller hangs up first: the callee is still joined, so the session stays
    // active and the caller goes to `left`.
    let ack = ca
        .cmd(json!({ "cmd": "calls.hangup", "payload": { "call_id": call_id } }))
        .await;
    assert_eq!(ack["ok"], json!(true), "A hangup: {ack}");
    for (label, c) in [("A", &mut ca), ("B", &mut cb)] {
        let ev = c.expect_evt(EVT_WAIT).await;
        assert_eq!(
            ev["payload"]["state"],
            json!("active"),
            "{label} still active: {ev}"
        );
        assert_eq!(part_state(&ev, a_char), "left", "{label} caller left: {ev}");
        assert_eq!(
            part_state(&ev, b_char),
            "joined",
            "{label} callee joined: {ev}"
        );
    }

    // Last party hangs up → session ends.
    let ack = cb
        .cmd(json!({ "cmd": "calls.hangup", "payload": { "call_id": call_id } }))
        .await;
    assert_eq!(ack["ok"], json!(true), "B hangup: {ack}");
    for (label, c) in [("A", &mut ca), ("B", &mut cb)] {
        let ev = c.expect_evt(EVT_WAIT).await;
        assert_eq!(
            ev["payload"]["state"],
            json!("ended"),
            "{label} ended: {ev}"
        );
    }
}

/// 2. `calls.start` rejections: self-call → invalid, unknown → not_found,
///    blocked → not_found *byte-identical* to unknown (privacy), busy → conflict.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn start_busy_and_block(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 16).await;
    let (token_a, a) = mint_full(&app, tenant, world, "a").await;
    let (_token_b, b) = mint_full(&app, tenant, world, "b").await;
    let (token_c, _c) = mint_full(&app, tenant, world, "c").await;
    let server = spawn_server(test_state(app, test_config()).await).await;

    let a_num = a.character.number.clone().expect("A number");
    let b_num = b.character.number.clone().expect("B number");

    let mut ca = connect_and_auth(server.addr, &token_a).await;
    let mut cc = connect_and_auth(server.addr, &token_c).await;

    // Self-call → invalid (resolve maps to self, caller == callee).
    let ack = calls_start(&mut ca, &a_num, false).await;
    assert_eq!(ack["err"]["code"], json!("invalid"), "self-call: {ack}");

    // Unknown number → not_found. Keep the err body to compare the blocked one.
    let ack_unknown = calls_start(&mut ca, "000-0000", false).await;
    assert_eq!(
        ack_unknown["err"]["code"],
        json!("not_found"),
        "unknown: {ack_unknown}"
    );

    // Busy: C calls B first (B now rings in a non-ended session), so A's call to
    // B is a conflict.
    let ack = calls_start(&mut cc, &b_num, false).await;
    assert_eq!(ack["ok"], json!(true), "C→B start: {ack}");
    let ack = calls_start(&mut ca, &b_num, false).await;
    assert_eq!(ack["err"]["code"], json!("conflict"), "busy callee: {ack}");

    // Blocked: A blocks B's number. Resolve happens before the busy check, so
    // even a busy-and-blocked callee reads exactly like an unknown number —
    // the whole err body must match (no existence/busy leak).
    let ack = ca
        .cmd(json!({ "cmd": "directory.block", "payload": { "number": b_num } }))
        .await;
    assert_eq!(ack["ok"], json!(true), "block B: {ack}");
    let ack_blocked = calls_start(&mut ca, &b_num, false).await;
    assert_eq!(
        ack_blocked["err"], ack_unknown["err"],
        "blocked must be byte-identical to unknown: {ack_blocked}"
    );
}

/// 3. Decline's session-end rule (§10.4 FSM): with the 1:1 caller still joined
///    the session stays ringing; with the caller already gone it ends. Both the
///    participant → declined and the session state are checked in the DB.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn decline_ends_or_continues(admin: PgPool) {
    let (state, ca, _a_num, cb, b_num) = state_and_two(&admin).await;
    let world = ca.world_id;

    // (a) Caller still joined → decline leaves the session ringing.
    let out = calls::start(&state, &ca, &b_num, false)
        .await
        .expect("start a");
    let call_a = parse_call_id(&out);
    calls::decline(&state, &cb, call_a)
        .await
        .expect("decline a");
    let (session, parts) = call_row(&state, world, call_a).await;
    assert_eq!(session, "ringing", "caller still joined keeps it alive");
    assert_eq!(
        state_of(&parts, cb.character_id),
        "declined",
        "callee declined"
    );
    assert_eq!(
        state_of(&parts, ca.character_id),
        "joined",
        "caller untouched"
    );

    // (b) Caller already gone → decline ends the session. In a real 1:1 the
    // caller's own hangup ends the ring first, so we seed the precondition
    // (caller `left`, session still ringing) to exercise decline's end branch.
    let out = calls::start(&state, &ca, &b_num, false)
        .await
        .expect("start b");
    let call_b = parse_call_id(&out);
    {
        let mut tx = world_tx(&state.pg, world).await.expect("tx");
        sqlx::query(
            "UPDATE call_participants SET state = 'left', left_at = now() \
             WHERE call_id = $1 AND character_id = $2",
        )
        .bind(call_b)
        .bind(ca.character_id)
        .execute(&mut *tx)
        .await
        .expect("force caller left");
        tx.commit().await.expect("commit");
    }
    calls::decline(&state, &cb, call_b)
        .await
        .expect("decline b");
    let (session, parts) = call_row(&state, world, call_b).await;
    assert_eq!(session, "ended", "no other active party → decline ends it");
    assert_eq!(
        state_of(&parts, cb.character_id),
        "declined",
        "callee declined"
    );
}

/// 4. Signal relay + authorization (§10.4): non-participant sender → forbidden,
///    non-participant `to` → forbidden, oversized payload → too_large, ended
///    call → conflict, and a valid signal relays with the right from/to.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn signal_relay_and_authz(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 16).await;
    let (token_a, a) = mint_full(&app, tenant, world, "a").await;
    let (token_b, b) = mint_full(&app, tenant, world, "b").await;
    let (token_c, c) = mint_full(&app, tenant, world, "c").await;
    let server = spawn_server(test_state(app, test_config()).await).await;

    let a_char = a.identity.character_id;
    let b_char = b.identity.character_id;
    let c_char = c.identity.character_id;
    let b_num = b.character.number.clone().expect("B number");

    let mut ca = connect_and_auth(server.addr, &token_a).await;
    let mut cb = connect_and_auth(server.addr, &token_b).await;
    let mut cc = connect_and_auth(server.addr, &token_c).await;

    let ack = calls_start(&mut ca, &b_num, false).await;
    assert_eq!(ack["ok"], json!(true), "start: {ack}");
    let call_id = call_id_of(&ack);

    // B subscribes to receive the relay; drain its snapshot-on-sub.
    let ack = cb.cmd(sub(format!("call:{call_id}"))).await;
    assert_eq!(ack["ok"], json!(true), "B call sub: {ack}");
    let snap = cb.expect_evt(EVT_WAIT).await;
    assert_eq!(snap["payload"]["state"], json!("ringing"), "B snap: {snap}");

    // A non-participant sender → forbidden.
    let ack = calls_signal(&mut cc, &call_id, b_char, json!({ "x": 1 })).await;
    assert_eq!(
        ack["err"]["code"],
        json!("forbidden"),
        "C (non-part) signal: {ack}"
    );

    // A `to` who is not a participant → forbidden.
    let ack = calls_signal(&mut ca, &call_id, c_char, json!({ "x": 1 })).await;
    assert_eq!(
        ack["err"]["code"],
        json!("forbidden"),
        "signal to non-part: {ack}"
    );

    // Oversized payload (> 16 KB) → too_large (checked before authz).
    let big = "x".repeat(17 * 1024);
    let ack = calls_signal(&mut ca, &call_id, b_char, json!({ "blob": big })).await;
    assert_eq!(ack["err"]["code"], json!("too_large"), "oversized: {ack}");

    // Valid signal relays verbatim with from=A, to=B.
    let ack = calls_signal(&mut ca, &call_id, b_char, json!({ "sdp": "answer" })).await;
    assert_eq!(ack["ok"], json!(true), "valid signal: {ack}");
    let ev = cb.expect_evt(EVT_WAIT).await;
    assert_eq!(ev["evt"], json!("calls.signal"), "relay evt: {ev}");
    assert_eq!(ev["payload"]["from"], json!(a_char), "from: {ev}");
    assert_eq!(ev["payload"]["to"], json!(b_char), "to: {ev}");
    assert_eq!(
        ev["payload"]["payload"]["sdp"],
        json!("answer"),
        "payload: {ev}"
    );
    assert_eq!(ev["payload"]["call_id"], json!(call_id), "call_id: {ev}");

    // End the call (caller cancels the ring), then a signal on it → conflict.
    let ack = ca
        .cmd(json!({ "cmd": "calls.hangup", "payload": { "call_id": call_id } }))
        .await;
    assert_eq!(ack["ok"], json!(true), "A hangup ends ring: {ack}");
    let ack = calls_signal(&mut ca, &call_id, b_char, json!({ "sdp": "late" })).await;
    assert_eq!(
        ack["err"]["code"],
        json!("conflict"),
        "signal on ended: {ack}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// THOROUGHNESS — FSM/DB conflict paths, sub authz, janitor, RLS, concurrency.
// ═══════════════════════════════════════════════════════════════════════════

/// FSM transition rejections at the store seam: illegal state → conflict,
/// missing call → not_found, non-participant → forbidden.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn transition_conflict_paths(admin: PgPool) {
    let (state, ca, _a_num, cb, b_num) = state_and_two(&admin).await;
    let out = calls::start(&state, &ca, &b_num, false)
        .await
        .expect("start");
    let call_id = parse_call_id(&out);

    // Accept a call you already joined (caller is `joined` at start) → conflict.
    assert!(
        matches!(
            calls::accept(&state, &ca, call_id).await,
            Err(Fail::Code(ErrCode::Conflict))
        ),
        "re-accept as joined caller must conflict",
    );

    // Hang up a call you never joined (callee still `ringing`) → conflict.
    assert!(
        matches!(
            calls::hangup(&state, &cb, call_id).await,
            Err(Fail::Code(ErrCode::Conflict))
        ),
        "hangup while ringing must conflict",
    );

    // A non-existent call → not_found.
    assert!(
        matches!(
            calls::accept(&state, &ca, Uuid::now_v7()).await,
            Err(Fail::Code(ErrCode::NotFound))
        ),
        "unknown call_id → not_found",
    );

    // A non-participant of a real call → forbidden (before any FSM check).
    let third = identity::mint_session(&state.pg, ca.tenant_id, ca.world_id, "third", None, 600)
        .await
        .expect("mint third")
        .identity;
    assert!(
        matches!(
            calls::accept(&state, &third, call_id).await,
            Err(Fail::Code(ErrCode::Forbidden))
        ),
        "non-participant → forbidden",
    );
}

/// `sub call:<id>` is participant-only (§10.4, CDR-6): a non-participant gets a
/// forbidden ack and NO snapshot; a participant gets snapshot-before-ack.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn sub_call_participant_only(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 16).await;
    let (token_a, a) = mint_full(&app, tenant, world, "a").await;
    let (token_b, b) = mint_full(&app, tenant, world, "b").await;
    let (token_c, _c) = mint_full(&app, tenant, world, "c").await;
    let server = spawn_server(test_state(app, test_config()).await).await;

    let b_num = b.character.number.clone().expect("B number");
    let a_char = a.identity.character_id;
    let b_char = b.identity.character_id;

    let mut ca = connect_and_auth(server.addr, &token_a).await;
    let ack = calls_start(&mut ca, &b_num, false).await;
    assert_eq!(ack["ok"], json!(true), "start: {ack}");
    let call_id = call_id_of(&ack);

    // Non-participant C: forbidden ack, and no snapshot leaks.
    let mut cc = connect_and_auth(server.addr, &token_c).await;
    let ack = cc.cmd(sub(format!("call:{call_id}"))).await;
    assert_eq!(ack["err"]["code"], json!("forbidden"), "C sub: {ack}");
    cc.expect_no_evt(SHORT).await;

    // Participant B: ok ack, with the snapshot pushed before it.
    let mut cb = connect_and_auth(server.addr, &token_b).await;
    let ack = cb.cmd(sub(format!("call:{call_id}"))).await;
    assert_eq!(ack["ok"], json!(true), "B sub: {ack}");
    let snap = cb.expect_evt(EVT_WAIT).await;
    assert_eq!(snap["evt"], json!("calls.state"), "B snap evt: {snap}");
    assert_eq!(
        snap["payload"]["state"],
        json!("ringing"),
        "B snap state: {snap}"
    );
    assert_eq!(part_state(&snap, a_char), "joined", "caller joined: {snap}");
    assert_eq!(
        part_state(&snap, b_char),
        "ringing",
        "callee ringing: {snap}"
    );
}

/// Janitor zombie-ring reap (§10.4): a session still `ringing` past 60 s is
/// force-ended regardless of the caller's stuck `joined` row (a ring only leaves
/// `ringing` via accept); a fresh ring and an `active` (answered) call are not.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn janitor_reaps_zombie_rings(admin: PgPool) {
    let (state, ca, _a_num, _cb, _b_num) = state_and_two(&admin).await;
    let world = ca.world_id;
    let who = ca.character_id;

    // zombie: old ring (caller `joined`, as production makes it) → reaped.
    // fresh:  young ring → spared by age.
    // active: old but answered (state `active`) → a real call, never reaped.
    let zombie = seed_call(&state, world, who, "ringing", "joined", 120).await;
    let fresh = seed_call(&state, world, who, "ringing", "joined", 5).await;
    let active = seed_call(&state, world, who, "active", "joined", 120).await;

    let snaps = calls::store::reap_zombie_rings(&state.pg, world)
        .await
        .expect("reap");

    // Exactly the zombie is reaped; the snapshot is the final `ended` state.
    assert_eq!(snaps.len(), 1, "only the zombie ring is reaped");
    let snap = &snaps[0];
    assert_eq!(snap.call_id, zombie, "reaped the zombie");
    assert_eq!(
        snap.state,
        CallSessionState::Ended,
        "reaped snapshot is ended"
    );
    assert_eq!(snap.kind, CallKind::Voice, "kind preserved");
    assert_eq!(snap.participants.len(), 1, "one participant");
    assert_eq!(
        snap.participants[0].state,
        CallParticipantState::Joined,
        "participant state left untouched by the reap",
    );

    // DB agrees: zombie ended, the fresh ring and the active call untouched.
    assert_eq!(
        call_row(&state, world, zombie).await.0,
        "ended",
        "zombie ended"
    );
    assert_eq!(
        call_row(&state, world, fresh).await.0,
        "ringing",
        "fresh spared (age)"
    );
    assert_eq!(
        call_row(&state, world, active).await.0,
        "active",
        "active call spared (not a ring)",
    );

    // Idempotent: a second sweep finds nothing to end.
    let again = calls::store::reap_zombie_rings(&state.pg, world)
        .await
        .expect("reap again");
    assert!(again.is_empty(), "already-ended rings are not re-reaped");
}

/// Regression (the adversarial review's keeper): an unanswered ring after a
/// *caller crash* must be reaped. `calls::start` always leaves the caller
/// `joined`, and a crashed caller's WS death does not transition its DB row —
/// so a reap keyed on "no joined participant" would skip every real ring (the
/// caller shields it), leaving it `ringing` forever and pinning both parties
/// busy (a griefing DoS). The reap keys on session `ringing` + age instead, so
/// this passes. See reflections 2026-07-18 (Sprint 6).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn zombie_ring_after_caller_crash_is_reaped(admin: PgPool) {
    let (state, ca, _a_num, _cb, b_num) = state_and_two(&admin).await;
    let world = ca.world_id;

    // A normal ring, exactly as production makes it: caller joined, callee
    // ringing.
    let out = calls::start(&state, &ca, &b_num, false)
        .await
        .expect("start");
    let call_id = parse_call_id(&out);

    // Simulate the caller's process dying mid-ring: the ring ages past 60s with
    // no accept and no hangup. Only created_at moves — the participant rows are
    // untouched, exactly as a crash leaves them (caller still `joined`).
    {
        let mut tx = world_tx(&state.pg, world).await.expect("tx");
        sqlx::query(
            "UPDATE call_sessions SET created_at = now() - interval '2 minutes' WHERE id = $1",
        )
        .bind(call_id)
        .execute(&mut *tx)
        .await
        .expect("age the ring");
        tx.commit().await.expect("commit");
    }

    let snaps = calls::store::reap_zombie_rings(&state.pg, world)
        .await
        .expect("reap");

    // The stale unanswered ring is force-ended even though the caller is still
    // `joined` (the crash never transitioned its row).
    assert_eq!(snaps.len(), 1, "the stale ring should be reaped");
    assert_eq!(
        call_row(&state, world, call_id).await.0,
        "ended",
        "a crashed caller's unanswered ring must not linger as `ringing`",
    );
}

/// Cross-world RLS isolation (mirrors `directory.rs::cross_world_rls_isolation`):
/// a call in world A is invisible under world B's tx. Raw unfiltered counts, so
/// a zero can only come from the policy, and the owning-world count proves the
/// rows exist (no vacuous pass).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn cross_world_rls_isolation(admin: PgPool) {
    let (state, ca, _a_num, _cb, b_num) = state_and_two(&admin).await;
    let world_a = ca.world_id;
    let (world_b, _tenant_b, _key_b) = seed_world_tenant(&admin).await;

    // One call in world A: 1 session, 2 participants.
    calls::start(&state, &ca, &b_num, false)
        .await
        .expect("start");

    let probes: [(&str, &str, i64); 2] = [
        ("call_sessions", "SELECT count(*) FROM call_sessions", 1),
        (
            "call_participants",
            "SELECT count(*) FROM call_participants",
            2,
        ),
    ];
    for (table, count_sql, expected) in probes {
        let mut tx_a = world_tx(&state.pg, world_a).await.expect("tx a");
        let in_a: i64 = sqlx::query_scalar(count_sql)
            .fetch_one(&mut *tx_a)
            .await
            .expect("count in world a");
        tx_a.commit().await.expect("commit a");
        assert_eq!(in_a, expected, "{table}: owning world sees its rows");

        let mut tx_b = world_tx(&state.pg, world_b).await.expect("tx b");
        let in_b: i64 = sqlx::query_scalar(count_sql)
            .fetch_one(&mut *tx_b)
            .await
            .expect("count in world b");
        tx_b.commit().await.expect("commit b");
        assert_eq!(in_b, 0, "{table}: cross-world read must be empty (RLS)");
    }
}

/// Concurrency canary (§10.4): the id-ordered session-then-participants
/// `FOR UPDATE` lock order means two concurrent transitions on the same call
/// serialize rather than deadlock. Two joined parties hang up at once → both
/// succeed, session ends, both `left`.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn concurrent_hangups_no_deadlock(admin: PgPool) {
    let (state, ca, _a_num, cb, b_num) = state_and_two(&admin).await;
    let world = ca.world_id;

    let out = calls::start(&state, &ca, &b_num, false)
        .await
        .expect("start");
    let call_id = parse_call_id(&out);
    calls::accept(&state, &cb, call_id).await.expect("accept"); // both joined, active

    let (r1, r2) = tokio::join!(
        calls::hangup(&state, &ca, call_id),
        calls::hangup(&state, &cb, call_id),
    );
    assert!(r1.is_ok(), "caller hangup: {r1:?}");
    assert!(r2.is_ok(), "callee hangup: {r2:?}");

    let (session, parts) = call_row(&state, world, call_id).await;
    assert_eq!(session, "ended", "last hangup ends the session");
    assert_eq!(state_of(&parts, ca.character_id), "left", "caller left");
    assert_eq!(state_of(&parts, cb.character_id), "left", "callee left");
}

/// Seed a `session_state` session `age_secs` old with one participant in
/// `part_state` (no `world_tx` INSERT grant issue — the RLS policy's USING
/// doubles as the INSERT WITH CHECK). Returns the call id.
async fn seed_call(
    state: &AppState,
    world: Uuid,
    participant: Uuid,
    session_state: &str,
    part_state: &str,
    age_secs: i64,
) -> Uuid {
    let call_id = Uuid::now_v7();
    let mut tx = world_tx(&state.pg, world).await.expect("tx");
    sqlx::query(
        "INSERT INTO call_sessions (id, world_id, kind, state, created_at) \
         VALUES ($1, $2, 'voice', $4, now() - make_interval(secs => $3))",
    )
    .bind(call_id)
    .bind(world)
    .bind(age_secs as f64)
    .bind(session_state)
    .execute(&mut *tx)
    .await
    .expect("seed session");
    sqlx::query(
        "INSERT INTO call_participants (call_id, world_id, character_id, state) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(call_id)
    .bind(world)
    .bind(participant)
    .bind(part_state)
    .execute(&mut *tx)
    .await
    .expect("seed participant");
    tx.commit().await.expect("commit");
    call_id
}
