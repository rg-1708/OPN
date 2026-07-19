//! Shared test support (reflections 2026-07-18): RLS-on testing means every
//! store assertion runs through a second pool connected as `opn_app` to the
//! per-test database `#[sqlx::test]` created with the admin role.

pub mod ws;

use std::net::SocketAddr;
use std::sync::Arc;

use opn_core::config::Config;
use opn_core::infra::auth::api_key_hash;
use opn_core::infra::ids::new_id;
use opn_core::state::AppState;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use uuid::Uuid;

/// Connection to the same per-test database, but as `opn_app`.
#[allow(dead_code)] // each test binary uses its own subset of this module
pub async fn app_pool(admin: &PgPool, max_connections: u32) -> PgPool {
    let db: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(admin)
        .await
        .expect("current_database");
    let mut url = url::Url::parse(&std::env::var("DATABASE_URL").expect("DATABASE_URL"))
        .expect("DATABASE_URL parses");
    url.set_username("opn_app").expect("set user");
    url.set_password(Some("opn")).expect("set password");
    url.set_path(&db);
    PgPoolOptions::new()
        .max_connections(max_connections)
        .connect(url.as_str())
        .await
        .expect("connect as opn_app")
}

/// Seeds a world + tenant (admin pool — `opn_app` has no INSERT grant on
/// either). Returns `(world_id, tenant_id, raw_api_key)`.
#[allow(dead_code)]
pub async fn seed_world_tenant(admin: &PgPool) -> (Uuid, Uuid, String) {
    let world_id = new_id();
    let tenant_id = new_id();
    let key = format!("opn_test_{}", new_id().simple());
    sqlx::query("INSERT INTO worlds (id, name) VALUES ($1, 'test-world')")
        .bind(world_id)
        .execute(admin)
        .await
        .expect("seed world");
    sqlx::query(
        "INSERT INTO tenants (id, name, api_key_hash, world_id) VALUES ($1, 'test-tenant', $2, $3)",
    )
    .bind(tenant_id)
    .bind(api_key_hash(&key))
    .bind(world_id)
    .execute(admin)
    .await
    .expect("seed tenant");
    (world_id, tenant_id, key)
}

/// Test `Config`: real Redis from the dev stack, everything else inert.
/// Fields the WS tests tune (queue depth, heartbeat) get overridden at the
/// call site before `Arc`-ing.
#[allow(dead_code)] // each test binary uses its own subset of this module
pub fn test_config() -> Config {
    Config {
        bind: "127.0.0.1:0".parse::<SocketAddr>().expect("addr"),
        metrics_bind: "127.0.0.1:0".parse::<SocketAddr>().expect("addr"),
        database_url: std::env::var("DATABASE_URL").unwrap_or_default(),
        migrate_database_url: String::new(),
        redis_url: std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".into()),
        s3_endpoint: String::new(),
        s3_bucket: String::new(),
        s3_key: String::new(),
        s3_secret: String::new(),
        s3_region: "us-east-1".into(),
        jwt_secret: "test".into(),
        session_ttl_secs: 600,
        replicas: 1,
        sendq_capacity: 256,
        preauth_global_max: 1000,
        preauth_per_ip_max: 5,
        heartbeat_secs: 30,
        ice_servers: serde_json::json!([]),
    }
}

/// Full `AppState` over the given pool (usually `app_pool`) with fresh
/// registry/limits/caps — the state every router/WS test starts from.
#[allow(dead_code)]
pub async fn test_state(pg: PgPool, cfg: Config) -> AppState {
    let client = redis::Client::open(cfg.redis_url.as_str()).expect("redis url");
    let redis = redis::aio::ConnectionManager::new(client)
        .await
        .expect("redis connect (dev stack up?)");
    let s3 = Arc::new(opn_core::infra::s3::S3::new(&cfg).expect("build s3 client"));
    AppState {
        pg,
        redis,
        registry: Arc::new(opn_core::gateway::registry::SessionRegistry::default()),
        links: Arc::new(opn_core::gateway::link::LinkRegistry::default()),
        limits: Arc::new(opn_core::infra::ratelimit::RateLimitTable::default()),
        preauth: Arc::new(opn_core::gateway::ws::PreauthCaps::default()),
        tenants: Arc::new(opn_core::infra::tenant_cache::TenantCache::default()),
        s3,
        cfg: Arc::new(cfg),
    }
}
