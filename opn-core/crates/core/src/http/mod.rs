use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::Router;
use metrics_exporter_prometheus::PrometheusHandle;

use crate::state::AppState;

pub mod tenant;

pub fn app_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/ws", get(crate::gateway::ws::ws_handler))
        .route("/v1/tenants/self/sessions", post(tenant::mint_session))
        .with_state(state)
}

pub fn metrics_router(handle: PrometheusHandle) -> Router {
    Router::new().route("/metrics", get(move || async move { handle.render() }))
}

/// Live PG `SELECT 1` + Redis `PING`, 1 s timeout each; 503 on any failure
/// (OPN-CORE.md §14). Coolify gates rollout on this.
async fn healthz(State(state): State<AppState>) -> (StatusCode, &'static str) {
    let pg = tokio::time::timeout(
        Duration::from_secs(1),
        sqlx::query("SELECT 1").execute(&state.pg),
    );
    let mut redis = state.redis.clone();
    let ping = tokio::time::timeout(Duration::from_secs(1), async move {
        redis::cmd("PING").query_async::<String>(&mut redis).await
    });
    let (pg, ping) = tokio::join!(pg, ping);
    match (pg, ping) {
        (Ok(Ok(_)), Ok(Ok(_))) => (StatusCode::OK, "ok"),
        (pg, ping) => {
            tracing::warn!(
                pg_ok = matches!(pg, Ok(Ok(_))),
                redis_ok = matches!(ping, Ok(Ok(_))),
                "healthz failing"
            );
            (StatusCode::SERVICE_UNAVAILABLE, "unavailable")
        }
    }
}
