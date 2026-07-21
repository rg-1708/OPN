//! calls primitive (OPN-CORE.md §10.4): voice/video call sessions with a
//! crash-proof FSM (`fsm.rs`), an opaque WebRTC signaling relay, and ring
//! delivery via `notify` (the dialer needs no standing sub). This module owns
//! validation + post-commit fan-out; `store.rs` owns the SQL.
//!
//! Sprint 6 part A is the WS-facing primitive. The tenant link that carries
//! voice-target events to the framework (`set_targets` on accept, `clear` on
//! end) and coturn `ice_servers` in the snapshot are part B — the accept/end
//! handlers here are where those link emits hook in.

pub mod fsm;
pub mod group;
pub mod store;

use contracts::{
    ActiveCall, CallKind, CallParticipantState, CallSessionState, ErrCode, Evt, NotifyClass,
    Topology, VoiceAction,
};
use serde_json::json;
use uuid::Uuid;

use super::notify::{self, Notification};
use super::Fail;
use crate::infra::auth::Identity;
use crate::state::AppState;
use fsm::Action;
use store::CallSnapshot;

/// Max phone-number length at the call seam (mirrors `channels::open_direct`).
const NUMBER_MAX: usize = 32;
/// Max opaque signaling payload (§10.4).
const SIGNAL_MAX_BYTES: usize = 16 * 1024;

/// Build the `calls.state` event from a snapshot — used by every emit path
/// (handlers here and the janitor reap). `ice_servers` is the static WebRTC ICE
/// config echoed into every snapshot (§5).
pub fn snapshot_evt(snap: &CallSnapshot, ice_servers: &serde_json::Value) -> Evt {
    // Topology picks the wire event: a group call gets `calls.group.state` (no
    // kind/ice — media rides the SFU), a 1:1 call gets `calls.state`. Both flow
    // through this one builder so every emit path (handlers + janitor) stays in
    // lockstep.
    match snap.topology {
        Topology::Sfu => Evt::CallsGroupState {
            call_id: snap.call_id,
            state: snap.state,
            participants: snap.participants.clone(),
            topology: Topology::Sfu,
        },
        Topology::P2p => Evt::CallsState {
            call_id: snap.call_id,
            kind: snap.kind,
            state: snap.state,
            participants: snap.participants.clone(),
            ice_servers: ice_servers.clone(),
            topology: Topology::P2p,
        },
    }
}

/// Publish the full `calls.state` snapshot on `call:<id>` AND update the tenant
/// link's voice targets (§5) — every state change routes through here so the two
/// stay in lockstep. Pub so the janitor reap emits the same pair.
pub async fn publish_snapshot(state: &AppState, world: Uuid, snap: &CallSnapshot) {
    crate::gateway::publish(
        state,
        world,
        &format!("call:{}", snap.call_id),
        &snapshot_evt(snap, &state.cfg.ice_servers),
    )
    .await;
    emit_voice(state, world, snap);
}

/// Push the voice-target event to the tenant link (§5): `set_targets` with the
/// joined characters while the call is active, `clear` when it ends. A ringing
/// call has no targets yet. Best-effort local send — a disconnected link drops
/// it and re-syncs on reconnect via `/calls/active`.
fn emit_voice(state: &AppState, world: Uuid, snap: &CallSnapshot) {
    // Group (SFU) calls carry no game-voice targets — media flows through the
    // LiveKit sidecar, not the tenant voice link. Only 1:1 (p2p) calls drive it.
    if snap.topology == Topology::Sfu {
        return;
    }
    let (action, characters) = match snap.state {
        CallSessionState::Active => (VoiceAction::SetTargets, joined_characters(snap)),
        CallSessionState::Ended => (VoiceAction::Clear, Vec::new()),
        // A ring has no voice targets yet — nothing to set or clear.
        CallSessionState::Ringing => return,
    };
    state.links.send(
        world,
        &Evt::CallsVoice {
            call_id: snap.call_id,
            action,
            characters,
        },
    );
}

fn joined_characters(snap: &CallSnapshot) -> Vec<Uuid> {
    snap.participants
        .iter()
        .filter(|p| p.state == CallParticipantState::Joined)
        .map(|p| p.character_id)
        .collect()
}

/// `calls.start` (§10.4): resolve → create a ringing session (caller joined,
/// callee ringing) → ring the callee via notify → `{ call_id }`.
pub async fn start(
    state: &AppState,
    who: &Identity,
    number: &str,
    video: bool,
) -> Result<serde_json::Value, Fail> {
    if number.is_empty() || number.len() > NUMBER_MAX {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    let kind = if video {
        CallKind::Video
    } else {
        CallKind::Voice
    };
    let out = store::start(
        &state.pg,
        who.world_id,
        who.character_id,
        who.device_id,
        number,
        kind,
    )
    .await?;

    // Ring the callee (notify picks live-push vs inbox). Best-effort: the
    // session exists regardless, and the janitor reaps an unanswered ring after
    // 60 s — a failed ring must not fail the caller's start.
    let n = Notification {
        app_id: "dialer".into(),
        kind: "incoming_call".into(),
        class: NotifyClass::Ring,
        payload: json!({
            "call_id": out.call_id,
            "caller_number": out.caller_number,
            "video": video,
        }),
    };
    if let Err(e) = notify::route(state, who.world_id, out.callee, n, false).await {
        tracing::error!(error = ?e, callee = %out.callee, "call ring notify failed");
    }

    Ok(json!({ "call_id": out.call_id }))
}

/// `calls.accept` (§10.4): FSM → session active, emit the fresh snapshot.
/// (Part B also emits `set_targets` on the tenant link here.)
pub async fn accept(state: &AppState, who: &Identity, call_id: Uuid) -> Result<(), Fail> {
    let snap = store::transition(
        &state.pg,
        who.world_id,
        call_id,
        who.character_id,
        Some(who.device_id),
        Action::Accept,
    )
    .await?;
    publish_snapshot(state, who.world_id, &snap).await;
    Ok(())
}

/// `calls.decline` (§10.4): FSM → declined (+ maybe ended), emit snapshot.
pub async fn decline(state: &AppState, who: &Identity, call_id: Uuid) -> Result<(), Fail> {
    transition_and_emit(state, who, call_id, Action::Decline).await
}

/// `calls.hangup` (§10.4): FSM → left (+ maybe ended), emit snapshot.
pub async fn hangup(state: &AppState, who: &Identity, call_id: Uuid) -> Result<(), Fail> {
    transition_and_emit(state, who, call_id, Action::Hangup).await
}

/// Shared body for decline/hangup: run the transition (device stays whatever it
/// was — only accept records a joining device) and emit the snapshot.
async fn transition_and_emit(
    state: &AppState,
    who: &Identity,
    call_id: Uuid,
    action: Action,
) -> Result<(), Fail> {
    let snap = store::transition(
        &state.pg,
        who.world_id,
        call_id,
        who.character_id,
        None,
        action,
    )
    .await?;
    publish_snapshot(state, who.world_id, &snap).await;
    Ok(())
}

/// `calls.signal` (§10.4): opaque relay. Authorize both parties as active
/// participants, then forward verbatim on `call:<id>` (durable). Never stored,
/// never inspected. Clients filter by `to`.
pub async fn signal(
    state: &AppState,
    who: &Identity,
    call_id: Uuid,
    to: Uuid,
    payload: serde_json::Value,
) -> Result<(), Fail> {
    if serde_json::to_vec(&payload)
        .map(|v| v.len())
        .unwrap_or(usize::MAX)
        > SIGNAL_MAX_BYTES
    {
        return Err(Fail::Code(ErrCode::TooLarge));
    }
    store::authorize_signal(&state.pg, who.world_id, call_id, who.character_id, to).await?;
    let evt = Evt::CallsSignal {
        call_id,
        from: who.character_id,
        to,
        payload,
    };
    crate::gateway::publish(state, who.world_id, &format!("call:{call_id}"), &evt).await;
    Ok(())
}

/// `sub call:<id>` authorization (§4.4, §10.4): participant-only. Called before
/// the dispatch arm registers the subscription.
pub async fn authorize_sub(state: &AppState, who: &Identity, call_id: Uuid) -> Result<(), Fail> {
    store::authorize_sub(&state.pg, who.world_id, call_id, who.character_id).await
}

/// The `calls.state` snapshot the dispatch arm pushes before the sub ack, read
/// *after* registration so a concurrent transition isn't lost.
pub async fn snapshot(state: &AppState, who: &Identity, call_id: Uuid) -> Result<Evt, Fail> {
    let snap = store::snapshot(&state.pg, who.world_id, call_id).await?;
    Ok(snapshot_evt(&snap, &state.cfg.ice_servers))
}

/// `GET /v1/tenants/self/calls/active` (§5): the tenant link's re-sync — every
/// non-ended session with its participants, so a reconnecting FXServer rebuilds
/// voice targets. Bounded by concurrent calls (no cursor).
pub async fn active_calls(state: &AppState, world: Uuid) -> Result<Vec<ActiveCall>, Fail> {
    let snaps = store::active_calls(&state.pg, world).await?;
    Ok(snaps
        .into_iter()
        .map(|s| ActiveCall {
            call_id: s.call_id,
            kind: s.kind,
            state: s.state,
            participants: s.participants,
        })
        .collect())
}
