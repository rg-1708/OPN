//! The pattern-proof the whole isolation story rests on (roadmap Sprint 0):
//! as the non-BYPASSRLS `opn_app` role, a query without `SET LOCAL
//! app.world_id` returns zero rows; inside `world_tx` it returns the seeded
//! row; a different world sees nothing.
//!
//! Requires the dev stack (`just dev`); `DATABASE_URL` must be a role that
//! can CREATE DATABASE (opn_migrate in the dev stack) — `#[sqlx::test]`
//! creates a per-test database and applies migrations to it.

use opn_core::infra::db::world_tx;
use opn_core::infra::ids::new_id;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

/// Connection to the same per-test database, but as `opn_app`.
async fn app_pool(admin: &PgPool) -> PgPool {
    let db: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(admin)
        .await
        .expect("current_database");
    let mut url = url::Url::parse(&std::env::var("DATABASE_URL").expect("DATABASE_URL"))
        .expect("DATABASE_URL parses");
    url.set_username("opn_app").expect("set user");
    url.set_password(Some("opn")).expect("set password");
    url.set_path(&db);
    PgPoolOptions::new()
        .max_connections(2)
        .connect(url.as_str())
        .await
        .expect("connect as opn_app")
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn rls_blocks_without_world_and_filters_by_world(admin: PgPool) {
    let app = app_pool(&admin).await;
    let world_a = new_id();
    let world_b = new_id();

    // Seed one row per world, as opn_app, through the mandatory helper.
    for world in [world_a, world_b] {
        let mut tx = world_tx(&app, world).await.expect("world_tx");
        sqlx::query("INSERT INTO _rls_canary (id, world_id, note) VALUES ($1, $2, 'seeded')")
            .bind(new_id())
            .bind(world)
            .execute(&mut *tx)
            .await
            .expect("insert canary row");
        tx.commit().await.expect("commit");
    }

    // No app.world_id set → zero rows, not an error.
    let rows = sqlx::query("SELECT * FROM _rls_canary")
        .fetch_all(&app)
        .await
        .expect("bare query must not error");
    assert!(
        rows.is_empty(),
        "RLS must hide all rows without app.world_id"
    );

    // Inside world_tx → exactly own world's row.
    let mut tx = world_tx(&app, world_a).await.expect("world_tx");
    let rows = sqlx::query("SELECT world_id FROM _rls_canary")
        .fetch_all(&mut *tx)
        .await
        .expect("scoped query");
    assert_eq!(rows.len(), 1, "exactly one row visible in world A");
    let got: uuid::Uuid = rows[0].get("world_id");
    assert_eq!(got, world_a);
    tx.rollback().await.expect("rollback");

    // Cross-world write must be rejected by WITH CHECK.
    let mut tx = world_tx(&app, world_a).await.expect("world_tx");
    let err =
        sqlx::query("INSERT INTO _rls_canary (id, world_id, note) VALUES ($1, $2, 'smuggled')")
            .bind(new_id())
            .bind(world_b)
            .execute(&mut *tx)
            .await;
    assert!(
        err.is_err(),
        "WITH CHECK must reject a row for another world"
    );
}
