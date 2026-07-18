//! Session mint through the real router (cross-cutting rule 3), plus the
//! cross-world RLS proof for every Sprint 1 table (exit criterion).

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{app_pool, seed_world_tenant};
use contracts::types::SessionMintResponse;
use opn_core::http::app_router;
use opn_core::infra::db::world_tx;
use opn_core::infra::ids::new_id;
use opn_core::state::AppState;
use sqlx::PgPool;
use tower::ServiceExt;

async fn state_over(app: PgPool) -> AppState {
    common::test_state(app, common::test_config()).await
}

async fn post_mint(state: &AppState, key: &str, body: &str) -> (StatusCode, Vec<u8>) {
    let res = app_router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/tenants/self/sessions")
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_owned()))
                .expect("request"),
        )
        .await
        .expect("oneshot");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .expect("body");
    (status, bytes.to_vec())
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn mint_route_happy_path_and_bad_key(admin: PgPool) {
    let (_, _, key) = seed_world_tenant(&admin).await;
    let state = state_over(app_pool(&admin, 4).await).await;

    let (status, _) = post_mint(&state, "opn_wrong", r#"{"framework_ref":"c1"}"#).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "wrong api key");

    let (status, body) = post_mint(&state, &key, r#"{"framework_ref":"c1"}"#).await;
    assert_eq!(status, StatusCode::OK);
    let minted: SessionMintResponse = serde_json::from_slice(&body).expect("response shape");
    assert!(!minted.token.is_empty());
    let number = minted.character.number.clone().expect("number assigned");
    assert!(number.starts_with("555-"));

    // Second mint: unknown framework_ref created the character above; the
    // same ref now reuses character + number, mints a fresh session.
    let (status, body) = post_mint(&state, &key, r#"{"framework_ref":"c1"}"#).await;
    assert_eq!(status, StatusCode::OK);
    let again: SessionMintResponse = serde_json::from_slice(&body).expect("response shape");
    assert_eq!(again.character.id, minted.character.id);
    assert_eq!(again.character.number.as_deref(), Some(number.as_str()));
    assert_ne!(again.session_id, minted.session_id);
}

/// Exit criterion: every new world-scoped table proves cross-world reads
/// return empty. Seeds a full object graph in world A, reads from world B.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn all_identity_tables_are_world_isolated(admin: PgPool) {
    let (world_a, tenant, _) = seed_world_tenant(&admin).await;
    let world_b = new_id();
    sqlx::query("INSERT INTO worlds (id, name) VALUES ($1, 'other')")
        .bind(world_b)
        .execute(&admin)
        .await
        .expect("seed world b");
    let app = app_pool(&admin, 4).await;

    let minted =
        opn_core::primitives::identity::mint_session(&app, tenant, world_a, "c1", None, 600)
            .await
            .expect("mint seeds characters/devices/sessions");
    let mut tx = world_tx(&app, world_a).await.expect("world_tx");
    sqlx::query(
        "INSERT INTO app_accounts (id, world_id, character_id, app_id, handle) \
         VALUES ($1, $2, $3, 'chirp', 'h')",
    )
    .bind(new_id())
    .bind(world_a)
    .bind(minted.identity.character_id)
    .execute(&mut *tx)
    .await
    .expect("seed app_account");
    sqlx::query("INSERT INTO retired_numbers (world_id, number) VALUES ($1, '555-0000')")
        .bind(world_a)
        .execute(&mut *tx)
        .await
        .expect("seed retired_number");
    tx.commit().await.expect("commit");

    let mut tx = world_tx(&app, world_b).await.expect("world_tx");
    for (table, sql) in [
        ("characters", "SELECT 1 FROM characters"),
        ("devices", "SELECT 1 FROM devices"),
        ("app_accounts", "SELECT 1 FROM app_accounts"),
        ("sessions", "SELECT 1 FROM sessions"),
        ("retired_numbers", "SELECT 1 FROM retired_numbers"),
    ] {
        let rows = sqlx::query(sql)
            .fetch_all(&mut *tx)
            .await
            .unwrap_or_else(|e| panic!("query {table}: {e}"));
        assert!(rows.is_empty(), "{table} leaked across worlds");
    }
}
