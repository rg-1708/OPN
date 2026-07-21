//! LiveKit webhook sink (opn-group-calls.md G1): `POST /v1/internal/livekit/webhook`.
//!
//! LiveKit is the source of truth for who is actually in a room. It POSTs
//! `participant_joined` / `participant_left` / `room_finished` events here; we
//! mirror them onto the participant rows and re-emit `calls.group.state`.
//!
//! Deviation from the design doc's "internal bind": this route lives on the
//! PUBLIC app_router, not the loopback admin bind — the admin bind is
//! unreachable from the LiveKit container in prod. The JWT signature (shared API
//! secret) over a body-hash claim is the actual trust boundary, so a public
//! route is safe; an unsigned/forged/tampered request is rejected 401.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use metrics::counter;
use uuid::Uuid;

use crate::primitives::calls;
use crate::state::AppState;

/// LiveKit webhook receiver. Verifies the signature + body hash, then truth-syncs
/// participant/room state idempotently. Always 200s a signed request (even an
/// unknown event or foreign room) so LiveKit does not retry; 401s an unsigned or
/// forged one; 404s when group calls are disabled.
pub async fn webhook(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let Some(lk) = state.cfg.livekit.as_ref() else {
        // Group calls disabled — the endpoint effectively does not exist.
        return StatusCode::NOT_FOUND;
    };

    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    // LiveKit sends the token bare in Authorization; tolerate a "Bearer " prefix.
    let token = auth.strip_prefix("Bearer ").unwrap_or(auth);
    if token.is_empty() || !calls::group::verify_webhook_body(&lk.api_secret, token, &body) {
        counter!("opn_livekit_webhook_total", "outcome" => "rejected").increment(1);
        tracing::warn!("livekit webhook rejected: signature/body-hash verification failed");
        return StatusCode::UNAUTHORIZED;
    }

    let Ok(parsed) = serde_json::from_slice::<calls::group::WebhookBody>(&body) else {
        // Signed but unparseable — ack so LiveKit stops retrying, log and drop.
        counter!("opn_livekit_webhook_total", "outcome" => "bad_body").increment(1);
        tracing::warn!("livekit webhook: signed but unparseable body");
        return StatusCode::OK;
    };

    handle_event(&state, parsed).await;
    counter!("opn_livekit_webhook_total", "outcome" => "ok").increment(1);
    StatusCode::OK
}

/// Route a verified webhook to the matching truth-sync. Unknown events, foreign
/// rooms, and unresolvable identities are ignored (the caller already 200s).
async fn handle_event(state: &AppState, body: calls::group::WebhookBody) {
    let room = body.room.as_ref().map(|r| r.name.as_str()).unwrap_or_default();
    let Some(call_id) = calls::group::call_id_from_room(room) else {
        return; // not one of our SFU rooms
    };
    match body.event.as_str() {
        "participant_joined" | "participant_left" => {
            let joined = body.event == "participant_joined";
            let Some(p) = body.participant.as_ref() else {
                return;
            };
            let Ok(character) = Uuid::parse_str(&p.identity) else {
                return; // identity is always a character uuid we minted; ignore junk
            };
            match calls::store::group_webhook_participant(&state.pg, call_id, character, joined).await
            {
                Ok(Some((world, snap))) => calls::publish_snapshot(state, world, &snap).await,
                Ok(None) => {}
                Err(e) => tracing::error!(error = %e, "livekit webhook participant sync failed"),
            }
        }
        "room_finished" => {
            match calls::store::group_webhook_room_finished(&state.pg, call_id).await {
                Ok(Some((world, snap))) => calls::publish_snapshot(state, world, &snap).await,
                Ok(None) => {}
                Err(e) => tracing::error!(error = %e, "livekit webhook room_finished failed"),
            }
        }
        _ => {} // unknown event → ignore
    }
}
