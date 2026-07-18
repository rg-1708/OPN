//! Sprint 3 notify tests (OPN-CORE.md §10.8): `route`'s live-push vs durable
//! inbox split, the `notify.seen`/`notify.clear` WS commands, the inbox HTTP
//! read, and cross-world RLS isolation. `route` runs against the same shared
//! registry the live server uses (AppState clones share it), so onlineness is
//! whatever the WS connections have registered.

mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::ws::{connect_and_auth, mint_token, spawn_server};
use common::{app_pool, seed_world_tenant, test_config, test_state};
use contracts::types::InboxItem;
use contracts::NotifyClass;
use opn_core::http::app_router;
use opn_core::infra::db::world_tx;
use opn_core::primitives::notify::{self, Notification};
use serde_json::json;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

const EVT_WAIT: Duration = Duration::from_secs(2);

fn sub(topic: String) -> serde_json::Value {
    json!({ "cmd": "sub", "payload": { "topic": topic } })
}

fn notif(class: NotifyClass, seq: i32) -> Notification {
    Notification {
        app_id: "messages".into(),
        kind: "message".into(),
        class,
        payload: json!({ "seq": seq }),
    }
}

/// `(id, class, seen)` for a character's inbox — read via world-scoped tx, the
/// only way past the FORCEd RLS on `inbox` (even the owner is filtered).
async fn inbox_rows(app: &PgPool, world: Uuid, character: Uuid) -> Vec<(Uuid, String, bool)> {
    let mut tx = world_tx(app, world).await.expect("world_tx");
    sqlx::query_as::<_, (Uuid, String, bool)>(
        "SELECT id, class, (seen_at IS NOT NULL) FROM inbox \
         WHERE character_id = $1 ORDER BY created_at, id",
    )
    .bind(character)
    .fetch_all(&mut *tx)
    .await
    .expect("query inbox")
}

// 1. Online recipient: `route` pushes `notify.event` on the subscribed
//    `notify:<device>` topic and writes no inbox row.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn route_pushes_to_online_recipient(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let (token, id) = mint_token(&app, tenant, world, "ref").await;
    let state = test_state(app.clone(), test_config()).await;
    let server = spawn_server(state.clone()).await;

    let mut c = connect_and_auth(server.addr, &token).await;
    let ack = c.cmd(sub(format!("notify:{}", id.device_id))).await;
    assert_eq!(ack["ok"], json!(true), "sub ack: {ack}");

    notify::route(
        &state,
        world,
        id.character_id,
        notif(NotifyClass::Alert, 1),
        false,
    )
    .await
    .expect("route");

    let ev = c.expect_evt(EVT_WAIT).await;
    assert_eq!(ev["evt"], json!("notify.event"), "push evt: {ev}");
    assert_eq!(ev["payload"]["app_id"], json!("messages"), "push: {ev}");

    assert!(
        inbox_rows(&app, world, id.character_id).await.is_empty(),
        "online path must not inbox"
    );
}

// 2. `notify.seen` marks the caller's own rows.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn seen_marks_rows(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let (token, id) = mint_token(&app, tenant, world, "ref").await;
    let state = test_state(app.clone(), test_config()).await;
    let server = spawn_server(state.clone()).await;

    // No connection yet → offline → both route calls inbox.
    for seq in 1..=2 {
        notify::route(
            &state,
            world,
            id.character_id,
            notif(NotifyClass::Alert, seq),
            false,
        )
        .await
        .expect("route");
    }
    let rows = inbox_rows(&app, world, id.character_id).await;
    assert_eq!(rows.len(), 2, "two inbox rows");
    let ids: Vec<Uuid> = rows.iter().map(|r| r.0).collect();

    let mut c = connect_and_auth(server.addr, &token).await;
    let ack = c
        .cmd(json!({ "cmd": "notify.seen", "payload": { "ids": ids } }))
        .await;
    assert_eq!(ack["ok"], json!(true), "seen ack: {ack}");

    let rows = inbox_rows(&app, world, id.character_id).await;
    assert!(rows.iter().all(|r| r.2), "both rows seen: {rows:?}");
}

// 3. `notify.clear` drops all the caller's rows.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn clear_empties_inbox(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let (token, id) = mint_token(&app, tenant, world, "ref").await;
    let state = test_state(app.clone(), test_config()).await;
    let server = spawn_server(state.clone()).await;

    for seq in 1..=3 {
        notify::route(
            &state,
            world,
            id.character_id,
            notif(NotifyClass::Alert, seq),
            false,
        )
        .await
        .expect("route");
    }
    assert_eq!(inbox_rows(&app, world, id.character_id).await.len(), 3);

    let mut c = connect_and_auth(server.addr, &token).await;
    let ack = c.cmd(json!({ "cmd": "notify.clear" })).await;
    assert_eq!(ack["ok"], json!(true), "clear ack: {ack}");

    assert!(
        inbox_rows(&app, world, id.character_id).await.is_empty(),
        "clear must empty the inbox"
    );
}

// 4. Offline + not muted: one inbox row, class preserved as `alert`.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn route_offline_inserts_inbox_alert(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 4).await;
    let (_, id) = mint_token(&app, tenant, world, "ref").await;
    let state = test_state(app.clone(), test_config()).await;

    notify::route(
        &state,
        world,
        id.character_id,
        notif(NotifyClass::Alert, 1),
        false,
    )
    .await
    .expect("route");

    let rows = inbox_rows(&app, world, id.character_id).await;
    assert_eq!(rows.len(), 1, "one inbox row");
    assert_eq!(rows[0].1, "alert", "class preserved");
}

// 5. Muted downgrades any class to `silent` before storing.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn route_muted_downgrades_to_silent(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 4).await;
    let (_, id) = mint_token(&app, tenant, world, "ref").await;
    let state = test_state(app.clone(), test_config()).await;

    notify::route(
        &state,
        world,
        id.character_id,
        notif(NotifyClass::Alert, 1),
        true,
    )
    .await
    .expect("route");

    let rows = inbox_rows(&app, world, id.character_id).await;
    assert_eq!(rows.len(), 1, "one inbox row");
    assert_eq!(rows[0].1, "silent", "muted → silent");
}

// 6. `GET /v1/notify/inbox` returns the stored item under the caller's JWT.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn inbox_http_returns_items(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 4).await;
    let (token, id) = mint_token(&app, tenant, world, "ref").await;
    let state = test_state(app.clone(), test_config()).await;

    notify::route(
        &state,
        world,
        id.character_id,
        notif(NotifyClass::Alert, 1),
        false,
    )
    .await
    .expect("route");

    let res = app_router(state.clone())
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/notify/inbox")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .expect("body");
    // Cursor idiom (Sprint 4): the inbox now returns `{ items, next_cursor }`.
    let page: serde_json::Value = serde_json::from_slice(&bytes).expect("inbox shape");
    let items: Vec<InboxItem> = serde_json::from_value(page["items"].clone()).expect("items array");
    assert!(page["next_cursor"].is_null(), "single page: {page}");

    assert_eq!(items.len(), 1, "one item: {items:?}");
    assert_eq!(items[0].app_id, "messages");
    assert_eq!(items[0].kind, "message");
    assert_eq!(items[0].class, NotifyClass::Alert);
}

// 7. Cross-world isolation: a row in world A is invisible to world B.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn inbox_rls_isolated(admin: PgPool) {
    let (world_a, tenant_a, _) = seed_world_tenant(&admin).await;
    let (world_b, tenant_b, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 4).await;
    let (_, id_a) = mint_token(&app, tenant_a, world_a, "a").await;
    let (_, id_b) = mint_token(&app, tenant_b, world_b, "b").await;
    let state = test_state(app.clone(), test_config()).await;

    notify::route(
        &state,
        world_a,
        id_a.character_id,
        notif(NotifyClass::Alert, 1),
        false,
    )
    .await
    .expect("route");

    assert_eq!(
        inbox_rows(&app, world_a, id_a.character_id).await.len(),
        1,
        "world A has the row"
    );
    assert!(
        inbox_rows(&app, world_b, id_b.character_id)
            .await
            .is_empty(),
        "world B must not see world A's inbox"
    );
}
