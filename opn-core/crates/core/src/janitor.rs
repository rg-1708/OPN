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
            run_task("message_partition", ensure_next_partition(&state.pg)).await;
            run_task("media_pending_reap", media_reap(&state)).await;
            run_task("media_verify", media_verify(&state)).await;
            run_task("listings_expire", listings_expire(&state.pg)).await;
            run_task("calls_reap", calls_reap(&state)).await;
            run_task("calls_reap_orphaned", calls_reap_orphaned(&state)).await;
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

/// Deletes expired listings (§10.7). Reads hide expired rows already, so this is
/// pure GC; SQL-only, so it rides the shared world-scoped sweep helper.
pub async fn listings_expire(pool: &PgPool) -> Result<u64> {
    sweep_worlds(
        pool,
        "listings_expire",
        "DELETE FROM listings WHERE expires_at IS NOT NULL AND expires_at < now()",
    )
    .await
}

/// Media pending-reap across every world (§10.6): each world's sweep does S3
/// DeleteObject calls after its DB delete, so it can't ride the SQL-only
/// `sweep_worlds` — it walks worlds itself, delegating to the media primitive.
async fn media_reap(state: &AppState) -> Result<u64> {
    let world_ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM worlds")
        .fetch_all(&state.pg)
        .await?;
    let mut total = 0u64;
    for world_id in world_ids {
        total += crate::primitives::media::reap_pending(state, world_id).await?;
    }
    Ok(total)
}

/// Media live-verification across every world (§10.6): HEADs objects, reverts
/// cap-bypassers/missing to `pending`. Same per-world walk as `media_reap`.
async fn media_verify(state: &AppState) -> Result<u64> {
    let world_ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM worlds")
        .fetch_all(&state.pg)
        .await?;
    let mut total = 0u64;
    for world_id in world_ids {
        total += crate::primitives::media::verify_live(state, world_id).await?;
    }
    Ok(total)
}

/// Reaps zombie call rings across every world (§10.4): un-accepted `ringing`
/// sessions older than 60 s are force-ended, and a final `calls.state` is
/// published so any live subscriber converges — plus a tenant-link `clear` (§5),
/// since a reaped session ends (`publish_snapshot` emits both). Per-world walk
/// (it publishes events after the SQL), like the media sweeps.
async fn calls_reap(state: &AppState) -> Result<u64> {
    let world_ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM worlds")
        .fetch_all(&state.pg)
        .await?;
    let mut total = 0u64;
    for world_id in world_ids {
        let snaps = crate::primitives::calls::store::reap_zombie_rings(&state.pg, world_id).await?;
        for snap in &snaps {
            crate::primitives::calls::publish_snapshot(state, world_id, snap).await;
        }
        total += snaps.len() as u64;
    }
    Ok(total)
}

/// Reaps orphaned *active* calls across every world (§10.4, §5): an active
/// session whose joined participants have all gone offline (no live WS
/// connection) — a double crash where neither party sent `hangup`, so no FSM
/// transition ever ends it. The registry (in-process presence) is the liveness
/// signal SQL can't see, so this bridges: the store yields active candidates +
/// their joined characters, we drop any with a still-online participant, and end
/// the rest through `publish_snapshot` (final `calls.state` + tenant-link
/// `clear`). Age-gated in the store so a fresh call is spared.
async fn calls_reap_orphaned(state: &AppState) -> Result<u64> {
    let world_ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM worlds")
        .fetch_all(&state.pg)
        .await?;
    let mut total = 0u64;
    for world_id in world_ids {
        let candidates =
            crate::primitives::calls::store::active_reap_candidates(&state.pg, world_id).await?;
        // Orphan = no joined participant is still online on this replica.
        let dead: Vec<Uuid> = candidates
            .into_iter()
            .filter(|(_, chars)| {
                chars
                    .iter()
                    .all(|c| !state.registry.is_character_online(world_id, *c))
            })
            .map(|(id, _)| id)
            .collect();
        let snaps =
            crate::primitives::calls::store::end_active_orphans(&state.pg, world_id, &dead).await?;
        for snap in &snaps {
            crate::primitives::calls::publish_snapshot(state, world_id, snap).await;
        }
        total += snaps.len() as u64;
    }
    Ok(total)
}

/// Stopgap `messages` partition maintenance (roadmap Sprint 3 item 2): create
/// next month's partition ahead of time so month-boundary writes never hit a
/// missing partition. NOT world-scoped — partitions are global — so no worlds
/// loop. The DDL runs via a SECURITY DEFINER function (opn_app cannot CREATE
/// TABLE; the function owner can); `IF NOT EXISTS` makes it idempotent and
/// safe to run every tick. Sprint 11 replaces this caller with pg_cron and
/// deletes the task (§9).
pub async fn ensure_next_partition(pool: &PgPool) -> Result<u64> {
    sqlx::query("SELECT ensure_message_partition(now() + interval '1 month')")
        .execute(pool)
        .await?;
    Ok(0)
}
