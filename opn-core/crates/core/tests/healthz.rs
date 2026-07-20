//! `/healthz` against the real router (`tower::ServiceExt::oneshot`,
//! roadmap cross-cutting rule 3). Requires the dev stack for Redis; the 503
//! case uses an unreachable Postgres instead of stopping a container so the
//! test is self-contained.

mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use opn_core::http::app_router;
use opn_core::state::AppState;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

async fn state_with(pg_url: &str) -> AppState {
    let pg = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_millis(500))
        .connect_lazy(pg_url)
        .expect("lazy pool");
    common::test_state(pg, common::test_config()).await
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

/// The body reports the running build's contracts version (Sprint 11 item 6) so
/// a deploy/triage can confirm which build is live from `/healthz` alone.
#[tokio::test]
async fn healthz_body_reports_contracts_version() {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL (dev stack up?)");
    let state = state_with(&url).await;
    let res = app_router(state)
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
        .await
        .expect("body bytes");
    let v: serde_json::Value = serde_json::from_slice(&bytes).expect("json body");
    assert_eq!(v["status"], "ok");
    assert_eq!(v["contracts_version"], contracts::CONTRACTS_VERSION);
}
