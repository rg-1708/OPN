//! Per-tenant config cache (roadmap Sprint 1 item 7). Hand-rolled 60 s TTL
//! over a std `RwLock` — deliberately not moka: at a handful of tenants the
//! whole map fits in a cache line's worth of pointers.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use sqlx::PgPool;
use uuid::Uuid;

const TTL: Duration = Duration::from_secs(60);

pub struct TenantCfg {
    pub name: String,
    pub world_id: Uuid,
    pub allowed_origins: Vec<String>,
}

#[derive(Default)]
pub struct TenantCache {
    // ponytail: unbounded growth is fine at <= dozens of tenants; a janitor
    // sweep can evict stale entries later if the tenant count ever explodes.
    inner: RwLock<HashMap<Uuid, (Arc<TenantCfg>, Instant)>>,
    /// Union of every tenant's origins — the pre-upgrade WS check (§4.1
    /// phase one; the tenant is unknown before the auth frame).
    union: RwLock<Option<(Arc<Vec<String>>, Instant)>>,
}

impl TenantCache {
    /// Cached entry if younger than 60 s, else refetch. `tenants` is not
    /// world-scoped (0003_identity.sql), so this reads the plain pool with
    /// only the column grants `opn_app` holds. Unknown tenant → `Ok(None)`,
    /// not cached. Never holds the lock across the await (std lock).
    pub async fn get(&self, pool: &PgPool, tenant_id: Uuid) -> Result<Option<Arc<TenantCfg>>> {
        if let Ok(map) = self.inner.read() {
            if let Some((cfg, at)) = map.get(&tenant_id) {
                if at.elapsed() < TTL {
                    return Ok(Some(cfg.clone()));
                }
            }
        }

        let row: Option<(String, Uuid, Vec<String>)> =
            sqlx::query_as("SELECT name, world_id, allowed_origins FROM tenants WHERE id = $1")
                .bind(tenant_id)
                .fetch_optional(pool)
                .await
                .context("tenant lookup")?;

        let Some((name, world_id, allowed_origins)) = row else {
            return Ok(None);
        };
        let cfg = Arc::new(TenantCfg {
            name,
            world_id,
            allowed_origins,
        });
        if let Ok(mut map) = self.inner.write() {
            map.insert(tenant_id, (cfg.clone(), Instant::now()));
        }
        Ok(Some(cfg))
    }

    /// Phase-one origin check: is `origin` allowed by *any* tenant? Coarse by
    /// design — the authoritative per-tenant re-check runs post-auth.
    pub async fn origin_allowed_any(&self, pool: &PgPool, origin: &str) -> Result<bool> {
        if let Ok(cached) = self.union.read() {
            if let Some((origins, at)) = cached.as_ref() {
                if at.elapsed() < TTL {
                    return Ok(origins.iter().any(|o| o == origin));
                }
            }
        }
        let rows: Vec<Vec<String>> = sqlx::query_scalar("SELECT allowed_origins FROM tenants")
            .fetch_all(pool)
            .await
            .context("origin union lookup")?;
        let origins: Arc<Vec<String>> = Arc::new(rows.into_iter().flatten().collect());
        if let Ok(mut cached) = self.union.write() {
            *cached = Some((origins.clone(), Instant::now()));
        }
        Ok(origins.iter().any(|o| o == origin))
    }
}
