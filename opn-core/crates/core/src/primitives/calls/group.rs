//! Group-call primitive (opn-group-calls.md G1): create/join/leave/end over the
//! LiveKit SFU. Core owns membership + the SFU room name; media never transits
//! Core. This module is the WS-facing seam — validation, the LiveKit access-token
//! mint, and post-commit `calls.group.state` fan-out; `store.rs` owns the SQL.
//!
//! Group calls fail closed when LiveKit is unconfigured (`state.cfg.livekit` is
//! `None`): every command answers `forbidden`, so a deploy without an SFU keeps
//! 1:1 calls and the data plane working.

use anyhow::Context;
use contracts::{CallSessionState, ErrCode, GroupJoinAck};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{publish_snapshot, store};
use crate::config::LivekitConfig;
use crate::infra::auth::Identity;
use crate::primitives::Fail;
use crate::state::AppState;
use base64::Engine;

/// LiveKit access-token lifetime: the client connects immediately, so 60 s is
/// ample and keeps a leaked token near-worthless (§G1).
const TOKEN_TTL_SECS: u64 = 60;

/// The active LiveKit config, or `forbidden` when group calls are disabled.
fn livekit(state: &AppState) -> Result<&LivekitConfig, Fail> {
    state
        .cfg
        .livekit
        .as_ref()
        .ok_or(Fail::Code(ErrCode::Forbidden))
}

/// `calls.group.create` (G1): any tenant member opens an SFU room and auto-joins.
/// `label`/`max_participants` are accepted but not persisted (no column; the
/// server cap is enforced at join). Ack `{ call_id }`.
pub async fn create(
    state: &AppState,
    who: &Identity,
    _label: Option<String>,
    _max_participants: Option<i64>,
) -> Result<serde_json::Value, Fail> {
    let lk = livekit(state)?;
    let snap = store::group_create(
        &state.pg,
        who.world_id,
        who.character_id,
        who.device_id,
        lk.max_rooms,
    )
    .await?;
    let call_id = snap.call_id;
    publish_snapshot(state, who.world_id, &snap).await;
    Ok(json!({ "call_id": call_id }))
}

/// `calls.group.join` (G1): membership check + a short-lived LiveKit token. The
/// client dials the SFU directly with it. Full room / ended → `conflict`,
/// non-existent → `not_found`. Ack `GroupJoinAck { sfu_url, token, expires_at }`.
pub async fn join(
    state: &AppState,
    who: &Identity,
    call_id: Uuid,
) -> Result<serde_json::Value, Fail> {
    let lk = livekit(state)?;
    // Mint before committing the membership row: a mint failure must not leave
    // a fanned-out "joined" phantom. Room names are deterministic (`grp_<id>`,
    // set at create), so the token can be built ahead of the join.
    // `sub` = the character id — the identity the tenant plane already uses
    // and the one LiveKit echoes back in webhooks.
    let now = now_secs();
    let token = mint_access_token(
        &lk.api_key,
        &lk.api_secret,
        &who.character_id.to_string(),
        &format!("grp_{call_id}"),
        now,
    )
    .map_err(Fail::Internal)?;
    let (snap, _room) = store::group_join(
        &state.pg,
        who.world_id,
        call_id,
        who.character_id,
        who.device_id,
        lk.max_participants_default,
    )
    .await?;
    publish_snapshot(state, who.world_id, &snap).await;
    let expires_at = OffsetDateTime::from_unix_timestamp((now + TOKEN_TTL_SECS) as i64)
        .ok()
        .and_then(|t| t.format(&Rfc3339).ok())
        .unwrap_or_default();
    let ack = GroupJoinAck {
        sfu_url: lk.url.clone(),
        token,
        expires_at,
    };
    Ok(serde_json::to_value(ack).map_err(anyhow::Error::from)?)
}

/// `calls.group.leave` (G1): participant → left; the last leave ends the room.
pub async fn leave(state: &AppState, who: &Identity, call_id: Uuid) -> Result<(), Fail> {
    livekit(state)?;
    let snap = store::group_leave(&state.pg, who.world_id, call_id, who.character_id).await?;
    publish_snapshot(state, who.world_id, &snap).await;
    Ok(())
}

/// `calls.group.end` (G1): creator-only teardown for everyone.
pub async fn end(state: &AppState, who: &Identity, call_id: Uuid) -> Result<(), Fail> {
    livekit(state)?;
    let snap = store::group_end(&state.pg, who.world_id, call_id, who.character_id).await?;
    publish_snapshot(state, who.world_id, &snap).await;
    Ok(())
}

// ── pure decisions (unit-tested without a DB) ────────────────────────────────

/// Join admission — the rule `store::group_join` enforces atomically under the
/// session lock. Ended session or a full room (`cap` other seats taken) →
/// `Conflict`; `ErrCode` is closed, so full and ended share it. Pure so the
/// branch is testable without Postgres.
pub fn join_admits(session: CallSessionState, others_joined: i64, cap: i64) -> Result<(), ErrCode> {
    if session == CallSessionState::Ended {
        return Err(ErrCode::Conflict);
    }
    if others_joined >= cap {
        return Err(ErrCode::Conflict);
    }
    Ok(())
}

/// Per-tenant room-cap admission (G3): a world holds at most `cap` concurrent
/// active group rooms — an anti-abuse ceiling so one tenant cannot spin up
/// unbounded SFU rooms. At/over the cap, `calls.group.create` answers `Conflict`
/// (end a room to free a slot), deliberately not `RateLimited`: no timed backoff
/// frees a slot, so the retry-after semantics would lie. Pure so it is testable
/// without Postgres.
pub fn rooms_admit(active_rooms: i64, cap: i64) -> Result<(), ErrCode> {
    if active_rooms >= cap {
        return Err(ErrCode::Conflict);
    }
    Ok(())
}

/// The room name for a call id, and its inverse — the sole coupling between a
/// call id and its LiveKit room. `sfu_room_id = "grp_<call id>"`.
pub fn call_id_from_room(name: &str) -> Option<Uuid> {
    name.strip_prefix("grp_")
        .and_then(|s| Uuid::parse_str(s).ok())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── LiveKit access token (JWT, HS256 — no LiveKit SDK) ───────────────────────

#[derive(Serialize)]
struct VideoGrant {
    room: String,
    #[serde(rename = "roomJoin")]
    room_join: bool,
    #[serde(rename = "canPublish")]
    can_publish: bool,
    #[serde(rename = "canSubscribe")]
    can_subscribe: bool,
}

#[derive(Serialize)]
struct AccessClaims {
    iss: String,
    sub: String,
    nbf: u64,
    exp: u64,
    video: VideoGrant,
}

/// Mint a LiveKit access token (G1): HS256 JWT signed with the API secret,
/// `iss` = API key, `sub` = participant identity, a 60 s window, and a `video`
/// grant scoped to `room` (join + publish + subscribe). Pure — the clock is the
/// `now_secs` argument — so the claims are unit-testable.
pub fn mint_access_token(
    api_key: &str,
    api_secret: &str,
    identity: &str,
    room: &str,
    now_secs: u64,
) -> anyhow::Result<String> {
    let claims = AccessClaims {
        iss: api_key.to_string(),
        sub: identity.to_string(),
        nbf: now_secs,
        exp: now_secs + TOKEN_TTL_SECS,
        video: VideoGrant {
            room: room.to_string(),
            room_join: true,
            can_publish: true,
            can_subscribe: true,
        },
    };
    jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(api_secret.as_bytes()),
    )
    .context("mint livekit access token")
}

// ── webhook signature verification ───────────────────────────────────────────

/// Parsed LiveKit webhook body (G1). Only the fields Core acts on; unknown
/// events carry neither `room` nor `participant` and are ignored upstream.
#[derive(Deserialize)]
pub struct WebhookBody {
    #[serde(default)]
    pub event: String,
    #[serde(default)]
    pub room: Option<WebhookRoom>,
    #[serde(default)]
    pub participant: Option<WebhookParticipant>,
}

#[derive(Deserialize)]
pub struct WebhookRoom {
    #[serde(default)]
    pub name: String,
}

#[derive(Deserialize)]
pub struct WebhookParticipant {
    #[serde(default)]
    pub identity: String,
}

#[derive(Deserialize)]
struct WebhookAuthClaims {
    sha256: String,
}

/// Verify a LiveKit webhook (G1). The `Authorization` header is a JWT signed
/// with the shared API secret whose `sha256` claim is the digest of the raw
/// body — signature + hash together are the trust boundary (the endpoint is on
/// the public router; see the doc deviation). Returns `false` on any failure:
/// bad signature, missing claim, or a hash that matches neither encoding.
///
/// LiveKit encodes `sha256` as the standard-base64 of the raw digest; we also
/// accept hex for robustness across SDK versions. Accepted-encoding note: the
/// base64 branch is the one LiveKit v1.x hits.
pub fn verify_webhook_body(api_secret: &str, auth_token: &str, body: &[u8]) -> bool {
    // Signature authenticates the sender; `exp` (always present on LiveKit
    // webhook tokens) bounds the replay window of a captured token.
    let mut v = Validation::new(Algorithm::HS256);
    v.validate_aud = false;
    v.required_spec_claims = std::iter::once("exp".to_string()).collect();
    let Ok(data) = jsonwebtoken::decode::<WebhookAuthClaims>(
        auth_token,
        &DecodingKey::from_secret(api_secret.as_bytes()),
        &v,
    ) else {
        return false;
    };
    let digest = Sha256::digest(body);
    let b64 = base64::engine::general_purpose::STANDARD.encode(digest);
    let hexed = hex::encode(digest);
    data.claims.sha256 == b64 || data.claims.sha256 == hexed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_admits_active_room_with_free_seats() {
        assert!(join_admits(CallSessionState::Active, 3, 32).is_ok());
        // A brand-new room (creator only) admits the next joiner.
        assert!(join_admits(CallSessionState::Active, 1, 32).is_ok());
    }

    #[test]
    fn join_rejects_full_and_ended() {
        // Full: `cap` other seats taken → conflict (== and > both reject).
        assert_eq!(
            join_admits(CallSessionState::Active, 32, 32),
            Err(ErrCode::Conflict)
        );
        assert_eq!(
            join_admits(CallSessionState::Active, 40, 32),
            Err(ErrCode::Conflict)
        );
        // Ended: no joining a dead room, even with free seats.
        assert_eq!(
            join_admits(CallSessionState::Ended, 0, 32),
            Err(ErrCode::Conflict)
        );
    }

    #[test]
    fn rooms_admit_enforces_tenant_cap() {
        assert!(rooms_admit(0, 50).is_ok());
        assert!(rooms_admit(49, 50).is_ok());
        // At and past the cap → conflict (end a room to free a slot).
        assert_eq!(rooms_admit(50, 50), Err(ErrCode::Conflict));
        assert_eq!(rooms_admit(51, 50), Err(ErrCode::Conflict));
    }

    #[test]
    fn room_name_roundtrips() {
        let id = Uuid::now_v7();
        assert_eq!(call_id_from_room(&format!("grp_{id}")), Some(id));
        assert_eq!(call_id_from_room("grp_not-a-uuid"), None);
        assert_eq!(call_id_from_room(&id.to_string()), None); // missing prefix
    }

    /// A minted token decodes with the secret and carries the LiveKit claims
    /// (iss = key, sub = identity, a 60 s window, and the `video` room grant).
    #[test]
    fn minted_token_carries_livekit_claims() {
        #[derive(Deserialize)]
        struct Video {
            room: String,
            #[serde(rename = "roomJoin")]
            room_join: bool,
            #[serde(rename = "canPublish")]
            can_publish: bool,
            #[serde(rename = "canSubscribe")]
            can_subscribe: bool,
        }
        #[derive(Deserialize)]
        struct Decoded {
            iss: String,
            sub: String,
            nbf: u64,
            exp: u64,
            video: Video,
        }

        let now = 1_700_000_000;
        let token =
            mint_access_token("devkey", "devsecret", "char-123", "grp_room", now).expect("mint");

        let mut v = Validation::new(Algorithm::HS256);
        v.validate_exp = false;
        v.required_spec_claims.clear();
        let data = jsonwebtoken::decode::<Decoded>(
            &token,
            &DecodingKey::from_secret(b"devsecret"),
            &v,
        )
        .expect("decode with secret");
        let c = data.claims;
        assert_eq!(c.iss, "devkey");
        assert_eq!(c.sub, "char-123");
        assert_eq!(c.nbf, now);
        assert_eq!(c.exp, now + TOKEN_TTL_SECS);
        assert_eq!(c.video.room, "grp_room");
        assert!(c.video.room_join && c.video.can_publish && c.video.can_subscribe);

        // A different secret must not verify the token.
        assert!(jsonwebtoken::decode::<Decoded>(
            &token,
            &DecodingKey::from_secret(b"wrong"),
            &v
        )
        .is_err());
    }

    /// Build a webhook auth token the way LiveKit does: sign a `sha256` claim
    /// (base64 of the body digest) with the API secret.
    fn sign_webhook(secret: &str, body: &[u8]) -> String {
        let sha256 = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body));
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &json!({ "sha256": sha256, "exp": now_secs() + 300 }),
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .expect("sign webhook")
    }

    #[test]
    fn webhook_verify_accepts_valid_rejects_forged_and_tampered() {
        let body = br#"{"event":"participant_joined","room":{"name":"grp_x"}}"#;

        // Valid: same secret, matching hash → accepted.
        let token = sign_webhook("devsecret", body);
        assert!(verify_webhook_body("devsecret", &token, body));

        // Forged: signed with a different secret → rejected.
        let forged = sign_webhook("attacker", body);
        assert!(!verify_webhook_body("devsecret", &forged, body));

        // Tampered: valid signature over the ORIGINAL body, but the body changed
        // → hash mismatch → rejected.
        let tampered = br#"{"event":"room_finished","room":{"name":"grp_x"}}"#;
        assert!(!verify_webhook_body("devsecret", &token, tampered));

        // Garbage token → rejected, no panic.
        assert!(!verify_webhook_body("devsecret", "not.a.jwt", body));
    }
}
