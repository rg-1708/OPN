use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use metrics_exporter_prometheus::PrometheusHandle;

use crate::state::AppState;

pub mod auth;
pub mod channels;
pub mod exchange;
pub mod feed;
pub mod ledger;
pub mod media;
pub mod notify;
pub mod tenant;

pub fn app_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/ws", get(crate::gateway::ws::ws_handler))
        .route("/link", get(crate::gateway::link::link_handler))
        .route("/v1/tenants/self/sessions", post(tenant::mint_session))
        .route("/v1/tenants/self/calls/active", get(tenant::active_calls))
        .route(
            "/v1/tenants/self/exchange",
            post(exchange::exchange).get(exchange::journal),
        )
        .route("/v1/notify/inbox", get(notify::inbox))
        .route("/v1/media", get(media::list))
        .route("/v1/channels/{id}/messages", get(channels::history))
        .route("/v1/ledger/history", get(ledger::history))
        .route("/v1/feed/home", get(feed::home))
        .route("/v1/feed/profile/{account}", get(feed::profile))
        .route("/v1/feed/posts/{id}", get(feed::post_detail))
        .route("/v1/feed/hashtags/{tag}", get(feed::hashtag))
        .with_state(state)
}

pub fn metrics_router(handle: PrometheusHandle) -> Router {
    Router::new().route("/metrics", get(move || async move { handle.render() }))
}

/// Live PG `SELECT 1` + Redis `PING`, 1 s timeout each; 503 on any failure
/// (OPN-CORE.md §14). Coolify gates rollout on this. The JSON body reports the
/// running build's `contracts_version` + `core_version` (roadmap Sprint 11
/// item 6) so a deploy/incident-triage can confirm which build is live without
/// shelling in — status flips to "unavailable" on the 503 path, versions stay.
async fn healthz(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let pg = tokio::time::timeout(
        Duration::from_secs(1),
        sqlx::query("SELECT 1").execute(&state.pg),
    );
    let mut redis = state.redis.clone();
    let ping = tokio::time::timeout(Duration::from_secs(1), async move {
        redis::cmd("PING").query_async::<String>(&mut redis).await
    });
    let (pg, ping) = tokio::join!(pg, ping);
    let body = |status: &str| {
        Json(serde_json::json!({
            "status": status,
            "contracts_version": contracts::CONTRACTS_VERSION,
            "core_version": env!("CARGO_PKG_VERSION"),
        }))
    };
    match (pg, ping) {
        (Ok(Ok(_)), Ok(Ok(_))) => (StatusCode::OK, body("ok")),
        (pg, ping) => {
            tracing::warn!(
                pg_ok = matches!(pg, Ok(Ok(_))),
                redis_ok = matches!(ping, Ok(Ok(_))),
                "healthz failing"
            );
            (StatusCode::SERVICE_UNAVAILABLE, body("unavailable"))
        }
    }
}
