//! Janitor v0 (roadmap Sprint 1 item 8): periodic world-scoped cleanup.
//!
//! `sessions` and `retired_numbers` are FORCE-RLS world-scoped tables, so a
//! pool-direct DELETE silently sees zero rows. Every task therefore iterates
//! worlds and deletes inside `world_tx`. A per-world advisory xact lock
//! serializes concurrent janitors per task; deletes are idempotent, so a
//! task dying mid-run is safe.

use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use metrics::counter;
use sqlx::PgPool;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, error, info_span, Instrument};
use uuid::Uuid;

use crate::state::AppState;

pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = interval(Duration::from_secs(30));
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            run_task("expired_sessions", expired_sessions(&state.pg)).await;
            run_task("retired_numbers_sweep", retired_numbers_sweep(&state.pg)).await;
            // In-process, no DB: buckets idle > 10 min go away (§12).
            run_task("ratelimit_sweep", async { Ok(state.limits.sweep_idle()) }).await;
        }
    })
}

/// Runs one task; a failure logs and counts but never escapes to kill the loop.
async fn run_task(name: &'static str, fut: impl Future<Output = Result<u64>>) {
    let span = info_span!("janitor", task = name);
    match fut.instrument(span).await {
        Ok(deleted) => {
            debug!(task = name, deleted, "janitor task ok");
            counter!("opn_janitor_runs_total", "task" => name, "outcome" => "ok").increment(1);
        }
        Err(err) => {
            error!(task = name, error = %err, "janitor task failed");
            counter!("opn_janitor_runs_total", "task" => name, "outcome" => "err").increment(1);
        }
    }
}

/// Walks every world, taking the per-task advisory lock then running
/// `delete_sql` inside that world's transaction. Returns total rows deleted.
async fn sweep_worlds(pool: &PgPool, task: &str, delete_sql: &'static str) -> Result<u64> {
    let world_ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM worlds")
        .fetch_all(pool)
        .await?;
    let lock_key = format!("janitor:{task}");
    let mut total = 0u64;
    for world_id in world_ids {
        let mut tx = crate::infra::db::world_tx(pool, world_id).await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
            .bind(&lock_key)
            .execute(&mut *tx)
            .await?;
        let deleted = sqlx::query(delete_sql)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        total += deleted;
    }
    Ok(total)
}

/// Drops long-expired sessions (kept 7 days past expiry for audit).
pub async fn expired_sessions(pool: &PgPool) -> Result<u64> {
    sweep_worlds(
        pool,
        "expired_sessions",
        "DELETE FROM sessions WHERE expires_at < now() - interval '7 days'",
    )
    .await
}

/// Drops retired numbers past their 30-day cooldown (no longer block reassign).
pub async fn retired_numbers_sweep(pool: &PgPool) -> Result<u64> {
    sweep_worlds(
        pool,
        "retired_numbers_sweep",
        "DELETE FROM retired_numbers WHERE freed_at < now() - interval '30 days'",
    )
    .await
}
