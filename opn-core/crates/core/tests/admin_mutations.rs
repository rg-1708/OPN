//! Sprint P1 (opn-panel-roadmap.md) admin mutations through the real
//! `admin_router`: create/rotate/freeze/unfreeze. Critical assertions: rotate
//! cuts the old key immediately and the new key authenticates; a frozen tenant
//! is refused at session mint; every mutation writes an audit row; the raw API
//! key never lands in an audit row. Key auth is exercised the real way — the
//! sha256 lookup on the `opn_app` pool that `TenantAuth` performs.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{app_pool, seed_world_tenant};
use contracts::ErrCode;
use opn_core::admin::rotate_tenant_key;
use opn_core::http::admin::{admin_router, AdminState};
use opn_core::infra::auth::{api_key_hash, mint_admin_jwt};
use opn_core::infra::ids::new_id;
use opn_core::infra::ratelimit::RateLimitTable;
use opn_core::primitives::{identity, Fail};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

fn admin_state(admin: PgPool) -> (AdminState, String) {
    let secret = "admin-test-secret".to_string();
    let (token, _) = mint_admin_jwt(&secret).expect("mint admin jwt");
    let state = AdminState {
        pg: admin,
        password_hash: Arc::new(String::new()),
        jwt_secret: Arc::new(secret),
        login_limits: Arc::new(RateLimitTable::default()),
    };
    (state, token)
}

async fn admin_req(
    state: &AdminState,
    token: &str,
    method: &str,
    uri: &str,
    body: &str,
) -> (StatusCode, Vec<u8>) {
    let res = admin_router(state.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_owned()))
                .expect("request"),
        )
        .await
        .expect("oneshot");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .expect("body");
    (status, bytes.to_vec())
}

/// The exact lookup `TenantAuth` runs, as `opn_app`: sha256 of the presented key.
async fn key_authenticates(app: &PgPool, key: &str) -> Option<Uuid> {
    sqlx::query_scalar("SELECT id FROM tenants WHERE api_key_hash = $1")
        .bind(api_key_hash(key))
        .fetch_optional(app)
        .await
        .expect("lookup")
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn create_returns_raw_key_once_and_audits_without_it(admin: PgPool) {
    let (state, token) = admin_state(admin.clone());

    let (status, body) = admin_req(&state, &token, "POST", "/admin/v1/tenants", r#"{"name":"acme"}"#)
        .await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_slice(&body).expect("json");
    let raw = v["api_key"].as_str().expect("api_key present").to_string();
    let fp = v["fingerprint"].as_str().expect("fingerprint").to_string();
    let tenant_id = Uuid::parse_str(v["id"].as_str().expect("id")).expect("uuid");
    assert!(raw.starts_with("opn_"));

    // The newly minted key authenticates via the real app-pool lookup.
    let app = app_pool(&admin, 2).await;
    assert_eq!(
        key_authenticates(&app, &raw).await,
        Some(tenant_id),
        "created key authenticates"
    );

    // Audit row exists and the raw key is nowhere in it.
    let (action, detail): (String, Value) =
        sqlx::query_as("SELECT action, detail FROM admin_audit WHERE target_tenant = $1")
            .bind(tenant_id)
            .fetch_one(&admin)
            .await
            .expect("audit row");
    assert_eq!(action, "tenant.create");
    let detail_str = serde_json::to_string(&detail).expect("detail json");
    assert!(!detail_str.contains(&raw), "raw key must never hit audit");
    assert!(detail_str.contains(&fp), "fingerprint is the safe handle");

    // GET tenants lists it as not frozen.
    let (status, body) = admin_req(&state, &token, "GET", "/admin/v1/tenants", "").await;
    assert_eq!(status, StatusCode::OK);
    let list: Value = serde_json::from_slice(&body).expect("list json");
    let row = list.as_array().expect("array").iter().next().expect("one tenant");
    assert_eq!(row["frozen"], Value::Bool(false));
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn rotate_cuts_old_key_immediately_and_audits(admin: PgPool) {
    let (_world, tenant, old_key) = seed_world_tenant(&admin).await;
    let (state, token) = admin_state(admin.clone());
    let app = app_pool(&admin, 2).await;

    assert_eq!(key_authenticates(&app, &old_key).await, Some(tenant));

    let (status, body) = admin_req(
        &state,
        &token,
        "POST",
        &format!("/admin/v1/tenants/{tenant}/rotate-key"),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let new_key = serde_json::from_slice::<Value>(&body).expect("json")["api_key"]
        .as_str()
        .expect("api_key")
        .to_string();

    assert!(
        key_authenticates(&app, &old_key).await.is_none(),
        "old key invalid immediately"
    );
    assert_eq!(
        key_authenticates(&app, &new_key).await,
        Some(tenant),
        "new key authenticates"
    );

    let action: String = sqlx::query_scalar(
        "SELECT action FROM admin_audit WHERE target_tenant = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(tenant)
    .fetch_one(&admin)
    .await
    .expect("audit row");
    assert_eq!(action, "tenant.rotate-key");

    // Unknown tenant → 404, and the shared fn returns None.
    let (status, _) = admin_req(
        &state,
        &token,
        "POST",
        &format!("/admin/v1/tenants/{}/rotate-key", new_id()),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(rotate_tenant_key(&admin, new_id())
        .await
        .expect("rotate unknown")
        .is_none());
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn freeze_refuses_mint_unfreeze_restores(admin: PgPool) {
    let (world, tenant, _key) = seed_world_tenant(&admin).await;
    let (state, token) = admin_state(admin.clone());
    let app = app_pool(&admin, 2).await;

    let (status, _) = admin_req(
        &state,
        &token,
        "POST",
        &format!("/admin/v1/tenants/{tenant}/freeze"),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let refused = identity::mint_session(&app, tenant, world, "c1", None, 600).await;
    assert!(
        matches!(refused, Err(Fail::Code(ErrCode::Forbidden))),
        "frozen tenant must be refused at mint with Forbidden"
    );

    let action: String = sqlx::query_scalar(
        "SELECT action FROM admin_audit WHERE target_tenant = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(tenant)
    .fetch_one(&admin)
    .await
    .expect("audit row");
    assert_eq!(action, "tenant.freeze");

    // List reflects the freeze.
    let (_s, body) = admin_req(&state, &token, "GET", "/admin/v1/tenants", "").await;
    let list: Value = serde_json::from_slice(&body).expect("list");
    assert_eq!(list.as_array().expect("array")[0]["frozen"], Value::Bool(true));

    let (status, _) = admin_req(
        &state,
        &token,
        "POST",
        &format!("/admin/v1/tenants/{tenant}/unfreeze"),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        identity::mint_session(&app, tenant, world, "c1", None, 600)
            .await
            .is_ok(),
        "unfrozen tenant can mint again"
    );

    // Unknown tenant → 404.
    let (status, _) = admin_req(
        &state,
        &token,
        "POST",
        &format!("/admin/v1/tenants/{}/freeze", new_id()),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
