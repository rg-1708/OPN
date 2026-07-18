//! Sprint 4 read-path tests (OPN-CORE.md §6, CDR-7): the channel-history HTTP
//! page (seq keyset on `before_seq`, membership-gated) and the notify inbox
//! opaque-cursor pagination. History is a bare `MessageItem[]` array ordered
//! seq DESC; inbox is the `{ items, next_cursor }` envelope, newest-first,
//! keyset on `(created_at, id)`. Messages are seeded straight into `messages`
//! via `world_tx` (WS `channels.send` is rate-limited); inbox rows are seeded
//! by routing to an offline character so `notify::route` takes the durable path.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::ws::{connect_and_auth, mint_full, mint_token, spawn_server, TestClient};
use common::{app_pool, seed_world_tenant, test_config, test_state};
use contracts::NotifyClass;
use opn_core::http::app_router;
use opn_core::infra::db::world_tx;
use opn_core::infra::ids::new_id;
use opn_core::primitives::notify::{self, Notification};
use opn_core::state::AppState;
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

/// `channels.open_direct` — returns the raw ack.
async fn open_direct(c: &mut TestClient, number: &str) -> Value {
    c.cmd(json!({ "cmd": "channels.open_direct", "payload": { "number": number } }))
        .await
}

/// Pull `payload.channel_id` off an ack, panicking with the ack on absence.
fn cid(ack: &Value) -> String {
    ack["payload"]["channel_id"]
        .as_str()
        .unwrap_or_else(|| panic!("no channel_id: {ack}"))
        .to_string()
}

/// One authed HTTP GET through the real router → `(status, parsed body)`.
async fn get(state: &AppState, uri: &str, token: &str) -> (StatusCode, Value) {
    let res = app_router(state.clone())
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("oneshot");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .expect("body");
    let body: Value = serde_json::from_slice(&bytes).expect("json body");
    (status, body)
}

/// The `seq` column of a history page (a bare `MessageItem[]`), in wire order.
fn seqs(body: &Value) -> Vec<i64> {
    body.as_array()
        .unwrap_or_else(|| panic!("history not an array: {body}"))
        .iter()
        .map(|m| m["seq"].as_i64().unwrap_or_else(|| panic!("no seq: {m}")))
        .collect()
}

/// The `items` array of an inbox page envelope, cloned out.
fn items(page: &Value) -> Vec<Value> {
    page["items"]
        .as_array()
        .unwrap_or_else(|| panic!("no items array: {page}"))
        .clone()
}

/// Seed `seqs` messages into `chan` directly (RLS-scoped world_tx), sender any
/// valid uuid — history reads seq/body, not membership of the sender.
async fn seed_messages(
    app: &PgPool,
    world: Uuid,
    chan: Uuid,
    sender: Uuid,
    seqs: std::ops::RangeInclusive<i64>,
) {
    let mut tx = world_tx(app, world).await.expect("world_tx");
    for seq in seqs {
        sqlx::query(
            "INSERT INTO messages \
               (id, world_id, channel_id, seq, sender_character, body, client_uuid) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(new_id())
        .bind(world)
        .bind(chan)
        .bind(seq)
        .bind(sender)
        .bind(json!({ "text": format!("m{seq}") }))
        .bind(new_id())
        .execute(&mut *tx)
        .await
        .expect("insert message");
    }
    tx.commit().await.expect("commit seed");
}

/// A `messages` notification carrying `seq` in its payload (so a page can be
/// checked for newest-first order without knowing the generated row ids).
fn notif(class: NotifyClass, seq: i32) -> Notification {
    Notification {
        app_id: "messages".into(),
        kind: "message".into(),
        class,
        payload: json!({ "seq": seq }),
    }
}

// 1. History keyset walk: seq DESC, `before_seq` pages the whole channel with
//    no dup/skip; a non-member is Forbidden.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn history_keyset_pagination(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 16).await;
    let (token_a, a) = mint_full(&app, tenant, world, "a").await;
    let (_token_b, b) = mint_full(&app, tenant, world, "b").await;
    let (token_c, _c) = mint_full(&app, tenant, world, "c").await;
    let state = test_state(app.clone(), test_config()).await;
    let server = spawn_server(state.clone()).await;

    // A opens a DM to B → the channel under test; A and B are its two members.
    let mut ca = connect_and_auth(server.addr, &token_a).await;
    let number_b = b.character.number.clone().expect("B number");
    let channel_id = cid(&open_direct(&mut ca, &number_b).await);
    let chan = Uuid::parse_str(&channel_id).expect("channel uuid");

    seed_messages(&app, world, chan, a.identity.character_id, 1..=7).await;

    // Page 1: newest 3, seq DESC.
    let (st, p1) = get(
        &state,
        &format!("/v1/channels/{channel_id}/messages?limit=3"),
        &token_a,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "page1 status: {p1}");
    assert_eq!(seqs(&p1), vec![7, 6, 5], "page1: {p1}");

    // Page 2: keyset past seq 5.
    let (_, p2) = get(
        &state,
        &format!("/v1/channels/{channel_id}/messages?before_seq=5&limit=3"),
        &token_a,
    )
    .await;
    assert_eq!(seqs(&p2), vec![4, 3, 2], "page2: {p2}");

    // Page 3: the tail is a short page.
    let (_, p3) = get(
        &state,
        &format!("/v1/channels/{channel_id}/messages?before_seq=2&limit=3"),
        &token_a,
    )
    .await;
    assert_eq!(seqs(&p3), vec![1], "page3: {p3}");

    // The full walk covers 1..=7 exactly — no duplicate, no gap.
    let mut walk: Vec<i64> = seqs(&p1)
        .into_iter()
        .chain(seqs(&p2))
        .chain(seqs(&p3))
        .collect();
    assert_eq!(walk.len(), 7, "walk length");
    walk.sort_unstable();
    assert_eq!(
        walk,
        (1..=7).collect::<Vec<i64>>(),
        "walk = 1..=7, no dup/skip"
    );

    // Non-member C sees the same channel_id as Forbidden (RLS: no leak).
    let (st, body) = get(
        &state,
        &format!("/v1/channels/{channel_id}/messages"),
        &token_c,
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "C status: {body}");
    assert_eq!(body["code"], json!("forbidden"), "C body: {body}");
}

// 2. `limit` is clamped (a 1000 request does not error and returns all rows);
//    an unknown/foreign channel id is Forbidden, not distinguishable from a
//    channel the caller simply isn't in.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn history_limit_and_membership(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 16).await;
    let (token_a, a) = mint_full(&app, tenant, world, "a").await;
    let (_token_b, b) = mint_full(&app, tenant, world, "b").await;
    let state = test_state(app.clone(), test_config()).await;
    let server = spawn_server(state.clone()).await;

    let mut ca = connect_and_auth(server.addr, &token_a).await;
    let number_b = b.character.number.clone().expect("B number");
    let channel_id = cid(&open_direct(&mut ca, &number_b).await);
    let chan = Uuid::parse_str(&channel_id).expect("channel uuid");

    seed_messages(&app, world, chan, a.identity.character_id, 1..=5).await;

    // limit=1000 → clamped to 100 by the handler: no error, all 5 returned.
    let (st, body) = get(
        &state,
        &format!("/v1/channels/{channel_id}/messages?limit=1000"),
        &token_a,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "big limit status: {body}");
    assert_eq!(seqs(&body), vec![5, 4, 3, 2, 1], "all 5, capped ok: {body}");

    // An unknown channel id (never a member) → Forbidden.
    let unknown = new_id();
    let (st, body) = get(
        &state,
        &format!("/v1/channels/{unknown}/messages"),
        &token_a,
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "unknown chan status: {body}");
    assert_eq!(
        body["code"],
        json!("forbidden"),
        "unknown chan body: {body}"
    );
}

// 3. Inbox cursor pages: 5 rows for an offline character walk out 2 + 2 + 1
//    across cursors, newest-first, no overlap; a garbage cursor is Invalid.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn inbox_cursor_pages(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    // Minting X does not open a WS session, so X stays offline and every route
    // takes the durable inbox path.
    let (token_x, x) = mint_token(&app, tenant, world, "x").await;
    let state = test_state(app.clone(), test_config()).await;

    for seq in 1..=5 {
        notify::route(
            &state,
            world,
            x.character_id,
            notif(NotifyClass::Alert, seq),
            false,
        )
        .await
        .expect("route");
    }

    // Page 1 confirms the rows landed in the inbox: 2 items + a next cursor.
    let (st, p1) = get(&state, "/v1/notify/inbox?limit=2", &token_x).await;
    assert_eq!(st, StatusCode::OK, "p1 status: {p1}");
    let items1 = items(&p1);
    assert_eq!(items1.len(), 2, "p1 len: {p1}");
    let c1 = p1["next_cursor"]
        .as_str()
        .expect("p1 next_cursor")
        .to_string();

    // Page 2: feed the cursor back.
    let (_, p2) = get(
        &state,
        &format!("/v1/notify/inbox?cursor={c1}&limit=2"),
        &token_x,
    )
    .await;
    let items2 = items(&p2);
    assert_eq!(items2.len(), 2, "p2 len: {p2}");
    let c2 = p2["next_cursor"]
        .as_str()
        .expect("p2 next_cursor")
        .to_string();

    // Page 3: the tail row, no further cursor.
    let (_, p3) = get(
        &state,
        &format!("/v1/notify/inbox?cursor={c2}&limit=2"),
        &token_x,
    )
    .await;
    let items3 = items(&p3);
    assert_eq!(items3.len(), 1, "p3 len: {p3}");
    assert!(p3["next_cursor"].is_null(), "p3 is the last page: {p3}");

    // The union of the three pages is all 5 distinct ids.
    let all: Vec<&Value> = items1.iter().chain(&items2).chain(&items3).collect();
    let mut ids: Vec<&str> = all
        .iter()
        .map(|i| i["id"].as_str().expect("item id"))
        .collect();
    assert_eq!(ids.len(), 5, "three pages hold 5 rows");
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 5, "all 5 ids distinct (no page overlap)");

    // Newest-first preserved across the cursor walk: seq 5,4,3,2,1.
    let walked_seqs: Vec<i64> = all
        .iter()
        .map(|i| i["payload"]["seq"].as_i64().expect("payload seq"))
        .collect();
    assert_eq!(
        walked_seqs,
        vec![5, 4, 3, 2, 1],
        "newest-first across pages"
    );

    // An unparseable cursor is Invalid (HTTP 400), never a 500 or a panic.
    let (st, body) = get(
        &state,
        "/v1/notify/inbox?cursor=not-a-real-cursor",
        &token_x,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "garbage cursor status: {body}");
    assert_eq!(
        body["code"],
        json!("invalid"),
        "garbage cursor body: {body}"
    );
}
