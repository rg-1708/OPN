//! config → migrations → pools → routers → serve (OPN-CORE.md §2).
//!
//! `expect` here is deliberate fail-fast startup (roadmap cross-cutting
//! rule 6: panics allowed in `main` and tests only).

use std::sync::Arc;
use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use tracing_subscriber::EnvFilter;

use opn_core::config::Config;
use opn_core::state::AppState;
use opn_core::{http, observe};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("admin") {
        if let Err(e) = opn_core::admin::run(&args[2..]).await {
            eprintln!("{e:#}");
            std::process::exit(1);
        }
        return;
    }

    let cfg = Config::from_env().expect("config");

    // Migrations run as the owner role; single-replica startup rule (§9).
    let migrate_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&cfg.migrate_database_url)
        .await
        .expect("connect migrate pool");
    opn_core::MIGRATOR
        .run(&migrate_pool)
        .await
        .expect("run migrations");
    migrate_pool.close().await;

    // Runtime pool: fail fast into an `internal` ack on exhaustion, never
    // queue forever (roadmap Sprint 0 item 4).
    let pg = PgPoolOptions::new()
        .max_connections(20)
        .acquire_timeout(Duration::from_secs(3))
        .connect(&cfg.database_url)
        .await
        .expect("connect app pool");

    let redis_client = redis::Client::open(cfg.redis_url.as_str()).expect("redis url");
    let redis = redis::aio::ConnectionManager::new(redis_client)
        .await
        .expect("connect redis");

    let prometheus = metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .expect("install metrics recorder");
    observe::register_metrics();

    let s3 = Arc::new(opn_core::infra::s3::S3::new(&cfg).expect("build s3 client"));

    let state = AppState {
        pg,
        redis,
        registry: Arc::new(opn_core::gateway::registry::SessionRegistry::default()),
        links: Arc::new(opn_core::gateway::link::LinkRegistry::default()),
        limits: Arc::new(opn_core::infra::ratelimit::RateLimitTable::default()),
        preauth: Arc::new(opn_core::gateway::ws::PreauthCaps::default()),
        tenants: Arc::new(opn_core::infra::tenant_cache::TenantCache::default()),
        s3,
        cfg: Arc::new(cfg),
    };

    opn_core::janitor::spawn(state.clone());
    opn_core::gateway::presence::spawn_refresher(state.clone());
    if state.cfg.replicas > 1 {
        opn_core::gateway::fanout::spawn_listener(state.clone());
    }

    let app_listener = opn_core::listener::NoDelayListener(
        tokio::net::TcpListener::bind(state.cfg.bind)
            .await
            .expect("bind OPN_BIND"),
    );
    let metrics_listener = tokio::net::TcpListener::bind(state.cfg.metrics_bind)
        .await
        .expect("bind OPN_METRICS_BIND");
    tracing::info!(bind = %state.cfg.bind, metrics = %state.cfg.metrics_bind, "opn-core up");

    // connect_info: the WS pre-auth per-IP cap needs the peer address.
    let app = axum::serve(
        app_listener,
        http::app_router(state)
            .into_make_service_with_connect_info::<opn_core::listener::ClientAddr>(),
    );
    let metrics = axum::serve(metrics_listener, http::metrics_router(prometheus));
    tokio::select! {
        r = app => r.expect("app server"),
        r = metrics => r.expect("metrics server"),
    }
}
