use std::sync::Arc;

use sqlx::PgPool;

use crate::config::Config;

/// Sprint 2 builds the real registry; the field exists now so `AppState`'s
/// shape is settled from day one.
#[derive(Debug, Default)]
pub struct SessionRegistry;

/// Sprint 2 builds the real token-bucket table.
#[derive(Debug, Default)]
pub struct RateLimitTable;

#[derive(Clone)]
pub struct AppState {
    pub pg: PgPool,
    pub redis: redis::aio::ConnectionManager,
    pub registry: Arc<SessionRegistry>,
    pub limits: Arc<RateLimitTable>,
    pub cfg: Arc<Config>,
}
