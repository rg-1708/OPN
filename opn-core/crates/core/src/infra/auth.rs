//! API-key hashing, JWT mint/verify, and the `Identity` every handler runs
//! as (OPN-CORE.md §11).
//!
//! `Identity` is constructible ONLY here (private `_priv` field): the single
//! path into one is `verify()`, which checks signature, expiry, and the live
//! session row. Handlers can never fabricate an identity from a payload —
//! the "never read identity from payload" rule (§7) held at the type level.

use anyhow::{Context, Result};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::infra::db::world_tx;

/// sha256 hex of a raw API key — the stored/looked-up form
/// (`tenants.api_key_hash`). High-entropy key, so the hash lookup is the
/// whole auth: no KDF, no constant-time compare needed.
pub fn api_key_hash(raw_key: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(raw_key.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Mint a fresh raw API key: 32 bytes of entropy → `opn_<url-safe-base64>` (43
/// chars, no padding). The single key-generation point — CLI `create-tenant`,
/// admin-panel create, and rotate-key all call it, so the entropy/format never
/// diverges. The raw key is shown once by the caller; only its `api_key_hash`
/// is stored, so it is unrecoverable after and MUST NOT be logged.
pub fn generate_api_key() -> String {
    use base64::Engine;
    use rand::RngCore;
    let mut buf = [0u8; 32];
    rand::rng().fill_bytes(&mut buf);
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf);
    format!("opn_{b64}")
}

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    sid: Uuid,
    tenant: Uuid,
    world: Uuid,
    #[serde(rename = "char")]
    character: Uuid,
    device: Uuid,
    exp: u64,
}

/// JWT lifetime (§11): 10 min, refreshed over the live WS. Independent of
/// the session TTL, which governs the `sessions` row.
const JWT_TTL_SECS: u64 = 600;

pub fn mint_jwt(secret: &str, identity_of: &Identity) -> Result<String> {
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock before epoch")?
        .as_secs()
        + JWT_TTL_SECS;
    let claims = Claims {
        sid: identity_of.session_id,
        tenant: identity_of.tenant_id,
        world: identity_of.world_id,
        character: identity_of.character_id,
        device: identity_of.device_id,
        exp,
    };
    jsonwebtoken::encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .context("encode jwt")
}

/// Admin-panel JWT (opn-panel-roadmap.md Sprint P0). A deliberately DIFFERENT
/// claim shape from `Claims`: it carries `sub: "admin"` and none of the session
/// fields. Combined with a separate signing secret (`ADMIN_JWT_SECRET`), an
/// admin token can never verify as a tenant session — `verify()` above requires
/// `sid`/`tenant`/`world`, which this token lacks — and a tenant token can
/// never verify here (missing `sub`, wrong signature).
#[derive(Debug, Serialize, Deserialize)]
struct AdminClaims {
    sub: String,
    exp: u64,
}

/// Admin JWT TTL: 30 min (roadmap §Admin authentication). Held in memory by the
/// SPA; re-login on refresh is acceptable.
const ADMIN_JWT_TTL_SECS: u64 = 1800;

/// Mint an admin token. Returns `(token, expires_at)` where `expires_at` is the
/// Unix-epoch second the token expires — the login response echoes it.
pub fn mint_admin_jwt(secret: &str) -> Result<(String, u64)> {
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock before epoch")?
        .as_secs()
        + ADMIN_JWT_TTL_SECS;
    let token = jsonwebtoken::encode(
        &Header::default(),
        &AdminClaims {
            sub: "admin".into(),
            exp,
        },
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .context("encode admin jwt")?;
    Ok((token, exp))
}

/// Verify an admin token: signature + `exp` (jsonwebtoken checks both) and the
/// `sub == "admin"` marker. Stateless — there is no admin session row. Returns
/// `VerifyError::Unauthorized` for any failure; there is no DB path, so
/// `Internal` never arises here.
pub fn verify_admin_jwt(secret: &str, token: &str) -> Result<(), VerifyError> {
    let data = jsonwebtoken::decode::<AdminClaims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    )
    .map_err(|_| VerifyError::Unauthorized)?;
    if data.claims.sub != "admin" {
        return Err(VerifyError::Unauthorized);
    }
    Ok(())
}

/// The authenticated actor behind a WS connection or JWT HTTP call. The only
/// constructor is [`verify`] (and [`Identity::for_new_session`], used once by
/// session mint, which by definition just created the row it attests to).
// Not #[non_exhaustive] (which clippy suggests): that only gates other
// crates; `_priv` gates construction inside this crate too, which is the
// invariant being enforced.
#[allow(clippy::manual_non_exhaustive)]
#[derive(Debug, Clone)]
pub struct Identity {
    pub session_id: Uuid,
    pub tenant_id: Uuid,
    pub world_id: Uuid,
    pub character_id: Uuid,
    pub device_id: Uuid,
    _priv: (),
}

impl Identity {
    /// For the mint path only: the session row was inserted in the same
    /// transaction, so there is nothing to re-verify.
    pub(crate) fn for_new_session(
        session_id: Uuid,
        tenant_id: Uuid,
        world_id: Uuid,
        character_id: Uuid,
        device_id: Uuid,
    ) -> Identity {
        Identity {
            session_id,
            tenant_id,
            world_id,
            character_id,
            device_id,
            _priv: (),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum VerifyError {
    /// Bad signature, expired, malformed — or the session row is revoked,
    /// expired, or gone. All the same to the caller: `unauthorized`.
    Unauthorized,
    /// DB trouble — distinct so callers ack `internal`, not `unauthorized`.
    Internal,
}

/// Signature + `exp`, then the live-session check (`revoked_at IS NULL AND
/// expires_at > now()`) — one indexed read (§11).
pub async fn verify(pool: &PgPool, secret: &str, token: &str) -> Result<Identity, VerifyError> {
    let data = jsonwebtoken::decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    )
    .map_err(|_| VerifyError::Unauthorized)?;
    let c = data.claims;

    let mut tx = world_tx(pool, c.world).await.map_err(|e| {
        tracing::error!(error = %e, "verify: world_tx failed");
        VerifyError::Internal
    })?;
    let live: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM sessions WHERE id = $1 AND revoked_at IS NULL AND expires_at > now()",
    )
    .bind(c.sid)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "verify: session lookup failed");
        VerifyError::Internal
    })?;
    if live.is_none() {
        return Err(VerifyError::Unauthorized);
    }

    Ok(Identity {
        session_id: c.sid,
        tenant_id: c.tenant,
        world_id: c.world,
        character_id: c.character,
        device_id: c.device,
        _priv: (),
    })
}
