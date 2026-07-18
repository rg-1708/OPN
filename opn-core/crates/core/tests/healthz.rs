//! `/healthz` against the real router (`tower::ServiceExt::oneshot`,
//! roadmap cross-cutting rule 3). Requires the dev stack for Redis; the 503
//! case uses an unreachable Postgres instead of stopping a container so the
//! test is self-contained.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use opn_core::config::Config;
use opn_core::http::app_router;
use opn_core::state::{AppState, RateLimitTable, SessionRegistry};
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

fn test_config() -> Config {
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
        jwt_secret: "test".into(),
        session_ttl_secs: 600,
        replicas: 1,
    }
}

async fn state_with(pg_url: &str) -> AppState {
    let cfg = test_config();
    let pg = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_millis(500))
        .connect_lazy(pg_url)
        .expect("lazy pool");
    let client = redis::Client::open(cfg.redis_url.as_str()).expect("redis url");
    let redis = redis::aio::ConnectionManager::new(client)
        .await
        .expect("redis connect (dev stack up?)");
    AppState {
        pg,
        redis,
        registry: Arc::new(SessionRegistry),
        limits: Arc::new(RateLimitTable),
        cfg: Arc::new(cfg),
    }
}

async fn get_healthz(state: AppState) -> StatusCode {
    let res = app_router(state)
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("oneshot");
    res.status()
}

#[tokio::test]
async fn healthz_ok_with_live_services() {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL (dev stack up?)");
    let state = state_with(&url).await;
    assert_eq!(get_healthz(state).await, StatusCode::OK);
}

#[tokio::test]
async fn healthz_503_when_postgres_unreachable() {
    let state = state_with("postgres://nobody:nothing@127.0.0.1:1/void").await;
    assert_eq!(get_healthz(state).await, StatusCode::SERVICE_UNAVAILABLE);
}
