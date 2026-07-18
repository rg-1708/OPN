use std::sync::Arc;

use sqlx::PgPool;

use crate::config::Config;
use crate::gateway::registry::SessionRegistry;
use crate::gateway::ws::PreauthCaps;
use crate::infra::ratelimit::RateLimitTable;
use crate::infra::s3::S3;
use crate::infra::tenant_cache::TenantCache;

#[derive(Clone)]
pub struct AppState {
    pub pg: PgPool,
    pub redis: redis::aio::ConnectionManager,
    pub registry: Arc<SessionRegistry>,
    pub limits: Arc<RateLimitTable>,
    pub preauth: Arc<PreauthCaps>,
    pub tenants: Arc<TenantCache>,
    pub s3: Arc<S3>,
    pub cfg: Arc<Config>,
}
