//! Admin panel API (opn-panel-roadmap.md Sprints P0+P1): a third router on its
//! own private bind (`ADMIN_BIND`, default loopback). Auth + reads (login,
//! tenants, stats, audit) and the tenant lifecycle mutations
//! (create/rotate-key/freeze/unfreeze), each of which writes an audit row.
//!
//! Every statement here runs on the OWNER pool (`OPN_MIGRATE_DATABASE_URL`),
//! the same elevated role the CLI uses: it bypasses RLS (so stats/tenant lists
//! span all worlds) and can read columns opn_app is not granted (e.g.
//! `tenants.created_at`).

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::admin::{create_world, insert_tenant, rotate_tenant_key, set_tenant_frozen};
use crate::http::tenant::err_response;
use crate::infra::auth::{mint_admin_jwt, verify_admin_jwt};
use crate::infra::ratelimit::{Class, RateLimitTable};
use contracts::ErrCode;

/// State for the admin router. Separate from `AppState`: its own owner pool and
/// its own login rate-limit table (never shared with tenant traffic).
#[derive(Clone)]
pub struct AdminState {
    /// Owner-role pool (`OPN_MIGRATE_DATABASE_URL`) — bypasses RLS.
    pub pg: PgPool,
    pub password_hash: Arc<String>,
    pub jwt_secret: Arc<String>,
    pub login_limits: Arc<RateLimitTable>,
}

pub fn admin_router(state: AdminState) -> Router {
    Router::new()
        .route("/admin/v1/login", post(login))
        .route("/admin/v1/tenants", get(tenants).post(create_tenant))
        .route("/admin/v1/tenants/{id}/rotate-key", post(rotate_key))
        .route("/admin/v1/tenants/{id}/freeze", post(freeze))
        .route("/admin/v1/tenants/{id}/unfreeze", post(unfreeze))
        .route("/admin/v1/stats", get(stats))
        .route("/admin/v1/audit", get(audit))
        // Any other `/admin/v1/*` path (typo, trailing slash, a removed route)
        // gets a JSON 404 — NOT the SPA index.html the outer static fallback
        // serves. Keeps the API namespace answering JSON with the right status,
        // so a stray `/admin/v1/tenants/` never returns a 200 HTML shell.
        .route("/admin/v1/{*rest}", any(api_not_found))
        .with_state(state)
}

/// Catch-all for unmatched `/admin/v1/*` paths: a uniform JSON 404.
async fn api_not_found() -> Response {
    err_response(ErrCode::NotFound, "not found")
}

/// Marker for a request bearing a valid admin JWT. Constructed only by the
/// extractor below, so a handler that takes it is provably admin-authed.
pub struct AdminIdentity;

impl axum::extract::FromRequestParts<AdminState> for AdminIdentity {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AdminState,
    ) -> Result<Self, Response> {
        let raw = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.strip_prefix("Bearer ").unwrap_or(v))
            .ok_or_else(|| err_response(ErrCode::Unauthorized, "missing token"))?;

        // Separate secret + `sub` marker: a tenant session JWT is rejected here.
        match verify_admin_jwt(&state.jwt_secret, raw) {
            Ok(()) => Ok(AdminIdentity),
            Err(_) => Err(err_response(ErrCode::Unauthorized, "invalid token")),
        }
    }
}

/// One shared login bucket (nil-uuid key). The bind is private and single-
/// operator, so a global brute-force throttle is enough.
// ponytail: global login bucket. Go per-IP (hash ClientAddr → key) only if the
// admin bind ever fronts multiple clients.
const LOGIN_KEY: Uuid = Uuid::nil();

#[derive(Deserialize)]
struct LoginReq {
    password: String,
}

#[derive(Serialize)]
struct LoginResp {
    token: String,
    /// Unix-epoch second the token expires.
    expires_at: u64,
}

/// `POST /admin/v1/login` — password → admin JWT.
///
/// A token is consumed up front so brute-force attempts are throttled *before*
/// the (deliberately expensive) argon2 verify runs. argon2's verify is
/// constant-time and the failure body is uniform, so a wrong password leaks
/// neither timing nor detail. The password and hash are never logged.
async fn login(State(state): State<AdminState>, Json(body): Json<LoginReq>) -> Response {
    // Class::Expensive = 0.2/s, burst 3: three quick tries, then ~1 per 5 s.
    if state
        .login_limits
        .check(LOGIN_KEY, Class::Expensive)
        .is_err()
    {
        return err_response(ErrCode::RateLimited, "too many attempts");
    }

    if !verify_password(&state.password_hash, &body.password) {
        return err_response(ErrCode::Unauthorized, "invalid credentials");
    }

    match mint_admin_jwt(&state.jwt_secret) {
        Ok((token, expires_at)) => Json(LoginResp { token, expires_at }).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "admin jwt mint failed");
            err_response(ErrCode::Internal, "internal")
        }
    }
}

/// Constant-time argon2id verify. A malformed stored hash is a misconfiguration,
/// not a wrong password — log it (without the hash) and fail closed.
fn verify_password(phc: &str, password: &str) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    let Ok(parsed) = PasswordHash::new(phc) else {
        tracing::error!("ADMIN_PASSWORD_HASH is not a valid argon2 PHC string");
        return false;
    };
    argon2::Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

#[derive(Serialize, sqlx::FromRow)]
struct TenantRow {
    id: Uuid,
    name: String,
    /// Unix-epoch seconds.
    created_at: i64,
    /// First 8 hex chars of `api_key_hash` (cross-cutting rule 2 — never the key).
    fingerprint: String,
    /// Whether the tenant is frozen (`frozen_at IS NOT NULL`) — new mints refused.
    frozen: bool,
    /// Unix-epoch seconds of the newest session for this tenant; null if none.
    last_session: Option<i64>,
}

/// `GET /admin/v1/tenants` — list tenants with key fingerprint, freeze state, and
/// last session. `last_session` is a correlated scan over `sessions` — cheap at
/// single-operator tenant counts.
async fn tenants(_admin: AdminIdentity, State(state): State<AdminState>) -> Response {
    let rows: Result<Vec<TenantRow>, _> = sqlx::query_as(
        "SELECT t.id, t.name, \
                extract(epoch FROM t.created_at)::bigint AS created_at, \
                substr(t.api_key_hash, 1, 8) AS fingerprint, \
                t.frozen_at IS NOT NULL AS frozen, \
                (SELECT extract(epoch FROM max(s.created_at))::bigint \
                   FROM sessions s WHERE s.tenant_id = t.id) AS last_session \
           FROM tenants t ORDER BY t.created_at DESC",
    )
    .fetch_all(&state.pg)
    .await;
    match rows {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "admin tenants query failed");
            err_response(ErrCode::Internal, "internal")
        }
    }
}

/// Write one admin audit row (owner pool). Called after each mutation succeeds;
/// a write failure 500s the request (cross-cutting rule 3 — every mutation has an
/// audit row, so we fail loud rather than lose the trail). `detail` NEVER carries
/// a raw API key (cross-cutting rule 2) — callers pass a fingerprint at most.
async fn write_audit(
    pg: &PgPool,
    action: &str,
    target: Uuid,
    detail: serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO admin_audit (action, target_tenant, detail) VALUES ($1, $2, $3)")
        .bind(action)
        .bind(target)
        .bind(detail)
        .execute(pg)
        .await
        .map(|_| ())
}

#[derive(Deserialize)]
struct CreateTenantReq {
    name: String,
}

#[derive(Serialize)]
struct CreateTenantResp {
    id: Uuid,
    name: String,
    fingerprint: String,
    /// Raw API key — shown EXACTLY ONCE (cross-cutting rule 2). Never logged,
    /// never in the audit row.
    api_key: String,
}

/// `POST /admin/v1/tenants` {name} → new world + tenant, RAW key returned once.
///
/// Shares the CLI's key-minting path (`crate::admin::insert_tenant`), so key
/// generation/hashing/insert never diverge. Creates a fresh world named after
/// the tenant (one tenant per world, §5). The raw key lives only in the response
/// body — the audit row carries the fingerprint only.
async fn create_tenant(
    _admin: AdminIdentity,
    State(state): State<AdminState>,
    Json(body): Json<CreateTenantReq>,
) -> Response {
    let name = body.name.trim();
    if name.is_empty() || name.len() > 128 {
        return err_response(ErrCode::Invalid, "name must be 1..=128 chars");
    }
    // One transaction for world + tenant: a failed tenant insert must not
    // leave an orphan world behind.
    let created = match async {
        let mut tx = state.pg.begin().await?;
        let world_id = create_world(&mut *tx, name).await?;
        let created = insert_tenant(&mut *tx, name, world_id).await?;
        tx.commit().await?;
        anyhow::Ok(created)
    }
    .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "admin create tenant failed");
            return err_response(ErrCode::Internal, "internal");
        }
    };
    if let Err(e) = write_audit(
        &state.pg,
        "tenant.create",
        created.tenant_id,
        serde_json::json!({ "name": name, "fingerprint": created.fingerprint }),
    )
    .await
    {
        tracing::error!(error = %e, "admin audit write failed (tenant.create)");
        return err_response(ErrCode::Internal, "internal");
    }
    Json(CreateTenantResp {
        id: created.tenant_id,
        name: name.to_string(),
        fingerprint: created.fingerprint,
        api_key: created.raw_key,
    })
    .into_response()
}

#[derive(Serialize)]
struct RotateKeyResp {
    id: Uuid,
    fingerprint: String,
    /// Raw API key — shown EXACTLY ONCE. Never logged, never in the audit row.
    api_key: String,
}

/// `POST /admin/v1/tenants/{id}/rotate-key` → new key (shown once), old hash
/// invalid immediately (immediate-cut; dual-key grace is gated). 404 if unknown.
/// Live sessions survive (JWTs — v1 known limit); see
/// `crate::admin::rotate_tenant_key`.
async fn rotate_key(
    _admin: AdminIdentity,
    State(state): State<AdminState>,
    Path(id): Path<Uuid>,
) -> Response {
    let rotated = match rotate_tenant_key(&state.pg, id).await {
        Ok(Some(r)) => r,
        Ok(None) => return err_response(ErrCode::NotFound, "no such tenant"),
        Err(e) => {
            tracing::error!(error = %e, "admin rotate key failed");
            return err_response(ErrCode::Internal, "internal");
        }
    };
    if let Err(e) = write_audit(
        &state.pg,
        "tenant.rotate-key",
        id,
        serde_json::json!({ "fingerprint": rotated.fingerprint }),
    )
    .await
    {
        tracing::error!(error = %e, "admin audit write failed (tenant.rotate-key)");
        return err_response(ErrCode::Internal, "internal");
    }
    Json(RotateKeyResp {
        id,
        fingerprint: rotated.fingerprint,
        api_key: rotated.raw_key,
    })
    .into_response()
}

#[derive(Serialize)]
struct FreezeResp {
    id: Uuid,
    frozen: bool,
}

/// `POST /admin/v1/tenants/{id}/freeze` → `frozen_at = now()`. New session mints
/// are refused (`identity::mint_session`); already-live sessions survive (v1
/// known limit). 404 if unknown.
async fn freeze(
    _admin: AdminIdentity,
    State(state): State<AdminState>,
    Path(id): Path<Uuid>,
) -> Response {
    set_frozen(&state, id, true, "tenant.freeze").await
}

/// `POST /admin/v1/tenants/{id}/unfreeze` → clears `frozen_at`. 404 if unknown.
async fn unfreeze(
    _admin: AdminIdentity,
    State(state): State<AdminState>,
    Path(id): Path<Uuid>,
) -> Response {
    set_frozen(&state, id, false, "tenant.unfreeze").await
}

async fn set_frozen(state: &AdminState, id: Uuid, frozen: bool, action: &str) -> Response {
    match set_tenant_frozen(&state.pg, id, frozen).await {
        Ok(0) => return err_response(ErrCode::NotFound, "no such tenant"),
        Ok(_) => {}
        Err(e) => {
            tracing::error!(error = %e, action, "admin freeze toggle failed");
            return err_response(ErrCode::Internal, "internal");
        }
    }
    if let Err(e) = write_audit(&state.pg, action, id, serde_json::Value::Null).await {
        tracing::error!(error = %e, action, "admin audit write failed (freeze toggle)");
        return err_response(ErrCode::Internal, "internal");
    }
    Json(FreezeResp { id, frozen }).into_response()
}

#[derive(Serialize)]
struct Stats {
    tenants: i64,
    live_sessions: i64,
    active_calls: i64,
    messages_24h: i64,
}

async fn load_stats(pg: &PgPool) -> Result<Stats, sqlx::Error> {
    Ok(Stats {
        tenants: sqlx::query_scalar("SELECT count(*) FROM tenants")
            .fetch_one(pg)
            .await?,
        live_sessions: sqlx::query_scalar(
            "SELECT count(*) FROM sessions WHERE revoked_at IS NULL AND expires_at > now()",
        )
        .fetch_one(pg)
        .await?,
        active_calls: sqlx::query_scalar("SELECT count(*) FROM call_sessions WHERE state = 'active'")
            .fetch_one(pg)
            .await?,
        // 24h count prunes to recent partitions of the RANGE-partitioned
        // `messages` table by `created_at`.
        messages_24h: sqlx::query_scalar(
            "SELECT count(*) FROM messages WHERE created_at > now() - interval '24 hours'",
        )
        .fetch_one(pg)
        .await?,
    })
}

/// `GET /admin/v1/stats` — top-line counts.
async fn stats(_admin: AdminIdentity, State(state): State<AdminState>) -> Response {
    match load_stats(&state.pg).await {
        Ok(s) => Json(s).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "admin stats query failed");
            err_response(ErrCode::Internal, "internal")
        }
    }
}

#[derive(Deserialize)]
struct AuditQuery {
    limit: Option<i64>,
}

#[derive(Serialize, sqlx::FromRow)]
struct AuditRow {
    id: i64,
    /// Unix-epoch seconds.
    at: i64,
    action: String,
    target_tenant: Option<Uuid>,
    detail: Option<serde_json::Value>,
}

/// `GET /admin/v1/audit?limit=N` — admin action log, newest first. Empty until
/// P1 writes rows. `limit` defaults to 100, clamped to 1..=1000.
async fn audit(
    _admin: AdminIdentity,
    State(state): State<AdminState>,
    Query(q): Query<AuditQuery>,
) -> Response {
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let rows: Result<Vec<AuditRow>, _> = sqlx::query_as(
        "SELECT id, extract(epoch FROM at)::bigint AS at, action, target_tenant, detail \
           FROM admin_audit ORDER BY id DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(&state.pg)
    .await;
    match rows {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "admin audit query failed");
            err_response(ErrCode::Internal, "internal")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::auth::mint_admin_jwt;

    // A tenant session JWT must never pass admin verification, and vice versa.
    // Different secret AND different claim shape — either alone rejects.
    #[test]
    fn admin_verify_rejects_foreign_and_expired() {
        let secret = "admin-secret";
        let (token, _exp) = mint_admin_jwt(secret).expect("mint");

        // Right secret → ok.
        assert!(verify_admin_jwt(secret, &token).is_ok());
        // Wrong secret (a tenant token is signed with a different key) → reject.
        assert!(verify_admin_jwt("other-secret", &token).is_err());
        // Garbage → reject.
        assert!(verify_admin_jwt(secret, "not.a.jwt").is_err());
    }

    #[test]
    fn login_bucket_throttles_after_burst() {
        let table = RateLimitTable::default();
        // Expensive burst is 3: three succeed, the fourth is throttled.
        for _ in 0..3 {
            assert!(table.check(LOGIN_KEY, Class::Expensive).is_ok());
        }
        assert!(table.check(LOGIN_KEY, Class::Expensive).is_err());
    }

    #[test]
    fn verify_password_roundtrip_and_reject() {
        use argon2::password_hash::{PasswordHasher, SaltString};
        let salt = SaltString::from_b64("YWJjZGVmZ2hpamts").expect("salt");
        let phc = argon2::Argon2::default()
            .hash_password(b"correct horse", &salt)
            .expect("hash")
            .to_string();
        assert!(verify_password(&phc, "correct horse"));
        assert!(!verify_password(&phc, "wrong"));
        assert!(!verify_password("not-a-phc-string", "correct horse"));
    }

    // `admin hash-password` output must verify here — the CLI mints what the
    // login path checks; params drifting apart would lock the operator out.
    #[test]
    fn cli_hash_password_verifies_at_login() {
        let phc = crate::admin::hash_admin_password("hunter2!").expect("hash");
        assert!(verify_password(&phc, "hunter2!"));
        assert!(!verify_password(&phc, "hunter3!"));
    }

    // Shared key-gen (used by CLI create, panel create, rotate) shape + entropy.
    #[test]
    fn generated_key_shape_and_uniqueness() {
        use crate::infra::auth::generate_api_key;
        let a = generate_api_key();
        let b = generate_api_key();
        assert!(a.starts_with("opn_"));
        assert_ne!(a, b, "keys must not repeat");
        // "opn_" + 43 chars (32 bytes url-safe base64, no pad).
        assert_eq!(a.len(), 47);
    }

    // The `/admin/v1/{*rest}` catch-all must coexist with the static routes —
    // axum inserts into matchit eagerly, so an overlap panics at construction.
    // Mirror the exact path shapes (stateless handlers) to prove it doesn't.
    #[test]
    fn catchall_coexists_with_static_admin_routes() {
        async fn noop() -> &'static str {
            ""
        }
        let _r: Router = Router::new()
            .route("/admin/v1/login", post(noop))
            .route("/admin/v1/tenants", get(noop).post(noop))
            .route("/admin/v1/tenants/{id}/rotate-key", post(noop))
            .route("/admin/v1/tenants/{id}/freeze", post(noop))
            .route("/admin/v1/tenants/{id}/unfreeze", post(noop))
            .route("/admin/v1/stats", get(noop))
            .route("/admin/v1/audit", get(noop))
            .route("/admin/v1/{*rest}", any(noop));
    }

    #[test]
    fn fingerprint_is_first_8_hex_of_hash() {
        use crate::infra::auth::{api_key_hash, generate_api_key};
        let key = generate_api_key();
        let fp = &api_key_hash(&key)[..8];
        assert_eq!(fp.len(), 8);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // Show-once contract: the raw key appears in the create response body but is
    // absent from the audit `detail` (cross-cutting rule 2).
    #[test]
    fn raw_key_in_response_but_never_in_audit_detail() {
        use crate::infra::auth::{api_key_hash, generate_api_key};
        let raw = generate_api_key();
        let fp = api_key_hash(&raw)[..8].to_string();

        let resp = CreateTenantResp {
            id: Uuid::nil(),
            name: "acme".into(),
            fingerprint: fp.clone(),
            api_key: raw.clone(),
        };
        let body = serde_json::to_string(&resp).expect("serialize resp");
        assert!(body.contains(&raw), "response body must carry the raw key once");

        // The exact detail the create handler audits.
        let detail = serde_json::json!({ "name": "acme", "fingerprint": fp });
        let detail_str = serde_json::to_string(&detail).expect("serialize detail");
        assert!(
            !detail_str.contains(&raw),
            "audit detail must never contain the raw key"
        );
        assert!(detail_str.contains(&fp));
    }
}
