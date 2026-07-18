//! Janitor tasks exercised through the `opn_app` pool (RLS on), the whole
//! point being that the per-world iteration is what makes the deletes visible.
//! Requires the dev stack; `DATABASE_URL` must be a CREATE DATABASE role.

use opn_core::infra::db::world_tx;
use opn_core::infra::ids::new_id;
use opn_core::janitor::{expired_sessions, retired_numbers_sweep};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use uuid::Uuid;

/// Connection to the same per-test database, but as `opn_app` (mirrors rls_canary).
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

/// worlds/tenants have no opn_app INSERT grant — seed via admin.
async fn seed_world(admin: &PgPool) -> (Uuid, Uuid) {
    let world = new_id();
    let tenant = new_id();
    sqlx::query("INSERT INTO worlds (id, name) VALUES ($1, 'w')")
        .bind(world)
        .execute(admin)
        .await
        .expect("seed world");
    sqlx::query("INSERT INTO tenants (id, name, api_key_hash, world_id) VALUES ($1, 't', $2, $3)")
        .bind(tenant)
        .bind(tenant.to_string())
        .bind(world)
        .execute(admin)
        .await
        .expect("seed tenant");
    (world, tenant)
}

/// Domain rows seed through opn_app inside the world transaction.
async fn seed_char_device(app: &PgPool, world: Uuid) -> (Uuid, Uuid) {
    let ch = new_id();
    let dev = new_id();
    let mut tx = world_tx(app, world).await.expect("world_tx");
    sqlx::query("INSERT INTO characters (id, world_id, framework_ref) VALUES ($1, $2, 'ref')")
        .bind(ch)
        .bind(world)
        .execute(&mut *tx)
        .await
        .expect("seed character");
    sqlx::query(
        "INSERT INTO devices (id, world_id, owner_character, kind) VALUES ($1, $2, $3, 'phone')",
    )
    .bind(dev)
    .bind(world)
    .bind(ch)
    .execute(&mut *tx)
    .await
    .expect("seed device");
    tx.commit().await.expect("commit");
    (ch, dev)
}

async fn insert_session(
    app: &PgPool,
    world: Uuid,
    tenant: Uuid,
    ch: Uuid,
    dev: Uuid,
    expires_ago: &str,
) -> Uuid {
    let id = new_id();
    let mut tx = world_tx(app, world).await.expect("world_tx");
    sqlx::query(
        "INSERT INTO sessions (id, tenant_id, world_id, character_id, device_id, expires_at) \
         VALUES ($1, $2, $3, $4, $5, now() - $6::interval)",
    )
    .bind(id)
    .bind(tenant)
    .bind(world)
    .bind(ch)
    .bind(dev)
    .bind(expires_ago)
    .execute(&mut *tx)
    .await
    .expect("seed session");
    tx.commit().await.expect("commit");
    id
}

async fn session_ids(app: &PgPool, world: Uuid) -> Vec<Uuid> {
    let mut tx = world_tx(app, world).await.expect("world_tx");
    let ids = sqlx::query_scalar("SELECT id FROM sessions")
        .fetch_all(&mut *tx)
        .await
        .expect("list sessions");
    tx.rollback().await.expect("rollback");
    ids
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn expired_sessions_deletes_only_long_expired(admin: PgPool) {
    let app = app_pool(&admin).await;
    let (world, tenant) = seed_world(&admin).await;
    let (ch, dev) = seed_char_device(&app, world).await;

    insert_session(&app, world, tenant, ch, dev, "8 days").await;
    let recent = insert_session(&app, world, tenant, ch, dev, "1 hour").await;

    let deleted = expired_sessions(&app).await.expect("run task");
    assert_eq!(deleted, 1, "only the 8-day-expired session should be swept");
    assert_eq!(session_ids(&app, world).await, vec![recent]);
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn retired_numbers_sweep_deletes_only_past_cooldown(admin: PgPool) {
    let app = app_pool(&admin).await;
    let (world, _tenant) = seed_world(&admin).await;

    let mut tx = world_tx(&app, world).await.expect("world_tx");
    sqlx::query("INSERT INTO retired_numbers (world_id, number, freed_at) VALUES ($1, '111', now() - interval '31 days')")
        .bind(world)
        .execute(&mut *tx)
        .await
        .expect("seed old retired");
    sqlx::query("INSERT INTO retired_numbers (world_id, number, freed_at) VALUES ($1, '222', now() - interval '1 day')")
        .bind(world)
        .execute(&mut *tx)
        .await
        .expect("seed fresh retired");
    tx.commit().await.expect("commit");

    let deleted = retired_numbers_sweep(&app).await.expect("run task");
    assert_eq!(deleted, 1, "only the past-cooldown number should be swept");

    let mut tx = world_tx(&app, world).await.expect("world_tx");
    let remaining: Vec<String> = sqlx::query_scalar("SELECT number FROM retired_numbers")
        .fetch_all(&mut *tx)
        .await
        .expect("list retired");
    tx.rollback().await.expect("rollback");
    assert_eq!(remaining, vec!["222".to_string()]);
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn expired_sessions_sweeps_every_world(admin: PgPool) {
    let app = app_pool(&admin).await;

    let (world_a, tenant_a) = seed_world(&admin).await;
    let (ch_a, dev_a) = seed_char_device(&app, world_a).await;
    insert_session(&app, world_a, tenant_a, ch_a, dev_a, "8 days").await;

    let (world_b, tenant_b) = seed_world(&admin).await;
    let (ch_b, dev_b) = seed_char_device(&app, world_b).await;
    insert_session(&app, world_b, tenant_b, ch_b, dev_b, "8 days").await;

    let deleted = expired_sessions(&app).await.expect("run task");
    assert_eq!(deleted, 2, "one run must cover both worlds");
    assert!(session_ids(&app, world_a).await.is_empty());
    assert!(session_ids(&app, world_b).await.is_empty());
}
