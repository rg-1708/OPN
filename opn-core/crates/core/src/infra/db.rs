use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

/// Opens the transaction every domain-table access must run in: `BEGIN` +
/// `SET LOCAL app.world_id` (via `set_config(..., true)` to stay
/// parameterized). RLS policies filter on that setting (migration 0001).
///
/// Hard convention (roadmap Sprint 0 item 8): reviewers reject any
/// pool-direct domain query — if it touches a world-scoped table, it goes
/// through here.
pub async fn world_tx(pool: &PgPool, world_id: Uuid) -> sqlx::Result<Transaction<'_, Postgres>> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT set_config('app.world_id', $1, true)")
        .bind(world_id.to_string())
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}
