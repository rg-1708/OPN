//! Sprint G1 group-call tests (opn-group-calls.md): the SFU session FSM at the
//! DB + primitive layer. The pure decisions (join admission, token mint, webhook
//! signature) are unit-tested in `primitives/calls/group.rs`; here we drive the
//! store/handler seam that needs Postgres.
//!
//! Direct-primitive style (à la `directory.rs`/`calls.rs`): RLS-on `opn_app`
//! pool, live Redis, handlers called directly — a `world_tx` read is a sharper
//! assertion than draining events. Requires DATABASE_URL/REDIS_URL (dev stack).

mod common;

use common::{app_pool, seed_world_tenant, test_config, test_state};
use contracts::ErrCode;
use opn_core::config::LivekitConfig;
use opn_core::infra::auth::Identity;
use opn_core::infra::db::world_tx;
use opn_core::primitives::{calls, identity, Fail};
use opn_core::state::AppState;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

/// State with group calls enabled (`max_participants_default` = `cap`) plus
/// `n` minted members in one world.
async fn group_state(
    admin: &PgPool,
    cap: i64,
    max_rooms: i64,
    n: usize,
) -> (AppState, Uuid, Vec<Identity>) {
    let (world, tenant, _key) = seed_world_tenant(admin).await;
    let pool = app_pool(admin, 8).await;
    let mut cfg = test_config();
    cfg.livekit = Some(LivekitConfig {
        url: "ws://localhost:7880".into(),
        api_key: "devkey".into(),
        api_secret: "devsecret".into(),
        empty_room_reap_secs: 300,
        max_participants_default: cap,
        max_rooms,
    });
    let state = test_state(pool, cfg).await;
    let mut members = Vec::with_capacity(n);
    for i in 0..n {
        let m = identity::mint_session(&state.pg, tenant, world, &format!("m{i}"), None, 600)
            .await
            .expect("mint member");
        members.push(m.identity);
    }
    (state, world, members)
}

fn parse_call_id(out: &Value) -> Uuid {
    out["call_id"].as_str().expect("call_id").parse().expect("uuid")
}

/// `(session_state, topology, sfu_room_id, [(character, participant_state)])`.
async fn call_row(
    state: &AppState,
    world: Uuid,
    call_id: Uuid,
) -> (String, String, Option<String>, Vec<(Uuid, String)>) {
    let mut tx = world_tx(&state.pg, world).await.expect("tx");
    let (session, topology, room): (String, String, Option<String>) =
        sqlx::query_as("SELECT state, topology, sfu_room_id FROM call_sessions WHERE id = $1")
            .bind(call_id)
            .fetch_one(&mut *tx)
            .await
            .expect("session");
    let parts: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT character_id, state FROM call_participants WHERE call_id = $1 ORDER BY character_id",
    )
    .bind(call_id)
    .fetch_all(&mut *tx)
    .await
    .expect("participants");
    tx.commit().await.expect("commit");
    (session, topology, room, parts)
}

fn state_of(parts: &[(Uuid, String)], who: Uuid) -> &str {
    parts
        .iter()
        .find(|(c, _)| *c == who)
        .map(|(_, s)| s.as_str())
        .unwrap_or_else(|| panic!("no participant {who}"))
}

/// create → active SFU room with creator joined; join → membership + a
/// GroupJoinAck with the SFU url + a token; leave the non-creator → left, room
/// stays active; last leave → ended.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn create_join_leave_lifecycle(admin: PgPool) {
    let (state, world, m) = group_state(&admin, 32, 50, 2).await;
    let (a, b) = (&m[0], &m[1]);

    let out = calls::group::create(&state, a, Some("crew".into()), None)
        .await
        .expect("create");
    let call_id = parse_call_id(&out);

    let (session, topology, room, parts) = call_row(&state, world, call_id).await;
    assert_eq!(session, "active");
    assert_eq!(topology, "sfu");
    assert_eq!(room.as_deref(), Some(format!("grp_{call_id}").as_str()));
    assert_eq!(state_of(&parts, a.character_id), "joined");

    // B joins → ack carries the SFU url + a non-empty token + expires_at.
    let ack = calls::group::join(&state, b, call_id).await.expect("join");
    assert_eq!(ack["sfu_url"], Value::from("ws://localhost:7880"));
    assert!(ack["token"].as_str().is_some_and(|t| !t.is_empty()), "token: {ack}");
    assert!(ack["expires_at"].as_str().is_some_and(|t| !t.is_empty()), "expires_at: {ack}");
    let (_, _, _, parts) = call_row(&state, world, call_id).await;
    assert_eq!(state_of(&parts, b.character_id), "joined");

    // B leaves → left, room still active (A joined).
    calls::group::leave(&state, b, call_id).await.expect("b leave");
    let (session, _, _, parts) = call_row(&state, world, call_id).await;
    assert_eq!(session, "active");
    assert_eq!(state_of(&parts, b.character_id), "left");

    // A (last joined) leaves → room ends.
    calls::group::leave(&state, a, call_id).await.expect("a leave");
    let (session, _, _, _) = call_row(&state, world, call_id).await;
    assert_eq!(session, "ended");
}

/// A full room rejects the next join with `Conflict` (ErrCode is closed — no
/// dedicated `Full`).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn join_full_room_conflicts(admin: PgPool) {
    // cap = 1: the creator fills the single seat.
    let (state, _world, m) = group_state(&admin, 1, 50, 2).await;
    let out = calls::group::create(&state, &m[0], None, None).await.expect("create");
    let call_id = parse_call_id(&out);
    let err = calls::group::join(&state, &m[1], call_id).await.expect_err("full");
    assert!(matches!(err, Fail::Code(ErrCode::Conflict)), "got {err:?}");
}

/// end is creator-only: a non-creator gets `Forbidden`, the creator ends it.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn end_is_creator_only(admin: PgPool) {
    let (state, world, m) = group_state(&admin, 32, 50, 2).await;
    let (a, b) = (&m[0], &m[1]);
    let out = calls::group::create(&state, a, None, None).await.expect("create");
    let call_id = parse_call_id(&out);
    calls::group::join(&state, b, call_id).await.expect("join");

    let err = calls::group::end(&state, b, call_id).await.expect_err("non-creator");
    assert!(matches!(err, Fail::Code(ErrCode::Forbidden)), "got {err:?}");

    calls::group::end(&state, a, call_id).await.expect("creator end");
    let (session, _, _, _) = call_row(&state, world, call_id).await;
    assert_eq!(session, "ended");
}

/// Joining an ended room → `Conflict`.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn join_ended_room_conflicts(admin: PgPool) {
    let (state, _world, m) = group_state(&admin, 32, 50, 2).await;
    let out = calls::group::create(&state, &m[0], None, None).await.expect("create");
    let call_id = parse_call_id(&out);
    calls::group::end(&state, &m[0], call_id).await.expect("end");
    let err = calls::group::join(&state, &m[1], call_id).await.expect_err("ended");
    assert!(matches!(err, Fail::Code(ErrCode::Conflict)), "got {err:?}");
}

/// Per-tenant concurrent-room cap (G3): at `max_rooms` active rooms the next
/// `create` gets `Conflict`; ending one frees a slot (the RLS-scoped count sees
/// only 'active' rooms, so an ended room does not hold a seat).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn create_at_room_cap_conflicts(admin: PgPool) {
    // Ceiling of one active room in the world.
    let (state, _world, m) = group_state(&admin, 32, 1, 1).await;
    let out = calls::group::create(&state, &m[0], None, None).await.expect("first room");
    let first = parse_call_id(&out);

    // Second create while the first is active → over the ceiling → Conflict.
    let err = calls::group::create(&state, &m[0], None, None).await.expect_err("at cap");
    assert!(matches!(err, Fail::Code(ErrCode::Conflict)), "got {err:?}");

    // End the first → its slot frees → create succeeds again.
    calls::group::end(&state, &m[0], first).await.expect("end frees slot");
    calls::group::create(&state, &m[0], None, None).await.expect("room freed");
}

/// Janitor reap: an active SFU room that went empty without a clean leave (all
/// participants 'left', session still 'active') and aged past the window is
/// force-ended.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn janitor_reaps_empty_group_room(admin: PgPool) {
    let (state, world, m) = group_state(&admin, 32, 50, 1).await;
    let out = calls::group::create(&state, &m[0], None, None).await.expect("create");
    let call_id = parse_call_id(&out);

    // Simulate the missed-leave case: participant left directly (not via the
    // leave handler, which would end the room), session left active + backdated.
    {
        let mut tx = world_tx(&state.pg, world).await.expect("tx");
        sqlx::query("UPDATE call_participants SET state = 'left' WHERE call_id = $1")
            .bind(call_id)
            .execute(&mut *tx)
            .await
            .expect("mark left");
        sqlx::query("UPDATE call_sessions SET created_at = now() - interval '10 minutes' WHERE id = $1")
            .bind(call_id)
            .execute(&mut *tx)
            .await
            .expect("backdate");
        tx.commit().await.expect("commit");
    }

    let snaps = calls::store::reap_empty_group_rooms(&state.pg, world, 300)
        .await
        .expect("reap");
    assert_eq!(snaps.len(), 1, "one empty room reaped");
    let (session, _, _, _) = call_row(&state, world, call_id).await;
    assert_eq!(session, "ended");
}

/// LiveKit `participant_left` webhook truth-sync: mirrors the row and ends the
/// room when it empties. Resolves the world from the call id (unauthenticated
/// webhook path).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn webhook_participant_left_syncs_and_ends(admin: PgPool) {
    let (state, world, m) = group_state(&admin, 32, 50, 1).await;
    let a = &m[0];
    let out = calls::group::create(&state, a, None, None).await.expect("create");
    let call_id = parse_call_id(&out);

    let synced = calls::store::group_webhook_participant(&state.pg, call_id, a.character_id, false)
        .await
        .expect("webhook")
        .expect("known call");
    assert_eq!(synced.0, world, "resolved world");
    let (session, _, _, parts) = call_row(&state, world, call_id).await;
    assert_eq!(state_of(&parts, a.character_id), "left");
    assert_eq!(session, "ended", "empty room ended by webhook");

    // Unknown call id → None (webhook 200s and ignores).
    let unknown = calls::store::group_webhook_room_finished(&state.pg, Uuid::now_v7())
        .await
        .expect("webhook");
    assert!(unknown.is_none());
}
