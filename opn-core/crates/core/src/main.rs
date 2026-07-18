//! config → migrations → pools → routers → serve (OPN-CORE.md §2).
//!
//! `expect` here is deliberate fail-fast startup (roadmap cross-cutting
//! rule 6: panics allowed in `main` and tests only).

use std::sync::Arc;
use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use tracing_subscriber::EnvFilter;

use opn_core::config::Config;
use opn_core::state::{AppState, RateLimitTable, SessionRegistry};
use opn_core::{http, observe};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

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

    let state = AppState {
        pg,
        redis,
        registry: Arc::new(SessionRegistry),
        limits: Arc::new(RateLimitTable),
        cfg: Arc::new(cfg),
    };

    let app_listener = tokio::net::TcpListener::bind(state.cfg.bind)
        .await
        .expect("bind OPN_BIND");
    let metrics_listener = tokio::net::TcpListener::bind(state.cfg.metrics_bind)
        .await
        .expect("bind OPN_METRICS_BIND");
    tracing::info!(bind = %state.cfg.bind, metrics = %state.cfg.metrics_bind, "opn-core up");

    let app = axum::serve(app_listener, http::app_router(state));
    let metrics = axum::serve(metrics_listener, http::metrics_router(prometheus));
    tokio::select! {
        r = app => r.expect("app server"),
        r = metrics => r.expect("metrics server"),
    }
}
