//! Sprint 8 **part B** feed read-surface tests (OPN-CORE.md §10.3, roadmap
//! items 3–4): the fan-out-on-read home timeline, profile timeline, post detail
//! with comments, and the hashtag page — all HTTP on the cursor idiom (CDR-7),
//! all driven through the real `app_router`. Rows are seeded straight through
//! `world_tx` (RLS-on `opn_app` pool) with explicit `created_at` so keyset order
//! is deterministic; the write plane (Sprint 8 part A) is tested in `feed.rs`.
//!
//! Also here: the 100 k-row EXPLAIN test (the home query rides `posts_home`, no
//! seq scan) and the recorded p95 — the two deferred part-A exit criteria.

mod common;

use common::ws::{mint_full, spawn_server};
use common::{app_pool, seed_world_tenant, test_config, test_state};
use opn_core::infra::auth::Identity;
use opn_core::infra::db::world_tx;
use opn_core::infra::ids::new_id;
use opn_core::primitives::{feed, identity};
use opn_core::state::AppState;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use sqlx::PgPool;
use time::OffsetDateTime;
use tower::ServiceExt;
use uuid::Uuid;

const APP: &str = "instapic";

// ── seeding helpers (direct, RLS-scoped) ─────────────────────────────────────

/// An app account for `who`'s character in `APP`, set **active** on the session.
async fn login_account(app: &PgPool, world: Uuid, who: &Identity, handle: &str) -> Uuid {
    login_account_in(app, world, who, APP, handle).await
}

/// Like `login_account` but for an explicit app slug — cross-app tests log the
/// same character into two apps.
async fn login_account_in(
    app: &PgPool,
    world: Uuid,
    who: &Identity,
    app_id: &str,
    handle: &str,
) -> Uuid {
    let id = seed_account_in(app, world, who.character_id, app_id, handle).await;
    identity::app_login(app, who, app_id, id)
        .await
        .expect("app_login");
    id
}

/// An app account for a character in `APP`, no session login.
async fn seed_account(app: &PgPool, world: Uuid, character: Uuid, handle: &str) -> Uuid {
    seed_account_in(app, world, character, APP, handle).await
}

async fn seed_account_in(
    app: &PgPool,
    world: Uuid,
    character: Uuid,
    app_id: &str,
    handle: &str,
) -> Uuid {
    let id = new_id();
    let mut tx = world_tx(app, world).await.expect("world_tx");
    sqlx::query(
        "INSERT INTO app_accounts (id, world_id, character_id, app_id, handle) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(world)
    .bind(character)
    .bind(app_id)
    .bind(handle)
    .execute(&mut *tx)
    .await
    .expect("seed app account");
    tx.commit().await.expect("commit");
    id
}

/// A post in `APP` authored by `author` at an explicit `created_at`.
async fn seed_post(
    app: &PgPool,
    world: Uuid,
    author: Uuid,
    body: Value,
    created_at: OffsetDateTime,
) -> Uuid {
    seed_post_in(app, world, APP, author, body, created_at).await
}

async fn seed_post_in(
    app: &PgPool,
    world: Uuid,
    app_id: &str,
    author: Uuid,
    body: Value,
    created_at: OffsetDateTime,
) -> Uuid {
    let id = new_id();
    let mut tx = world_tx(app, world).await.expect("world_tx");
    sqlx::query(
        "INSERT INTO posts (id, world_id, app_id, author_account, body, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(world)
    .bind(app_id)
    .bind(author)
    .bind(&body)
    .bind(created_at)
    .execute(&mut *tx)
    .await
    .expect("seed post");
    tx.commit().await.expect("commit");
    id
}

/// Force a post's denormalized counters (so a read test can assert a non-zero,
/// non-symmetric value — a swapped or zeroed counter field then fails).
async fn set_counts(app: &PgPool, world: Uuid, post_id: Uuid, likes: i32, comments: i32) {
    let mut tx = world_tx(app, world).await.expect("world_tx");
    sqlx::query("UPDATE posts SET like_count = $2, comment_count = $3 WHERE id = $1")
        .bind(post_id)
        .bind(likes)
        .bind(comments)
        .execute(&mut *tx)
        .await
        .expect("set counts");
    tx.commit().await.expect("commit");
}

async fn seed_follow(app: &PgPool, world: Uuid, follower: Uuid, followee: Uuid) {
    let mut tx = world_tx(app, world).await.expect("world_tx");
    sqlx::query(
        "INSERT INTO follows (world_id, app_id, follower_account, followee_account) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(world)
    .bind(APP)
    .bind(follower)
    .bind(followee)
    .execute(&mut *tx)
    .await
    .expect("seed follow");
    tx.commit().await.expect("commit");
}

async fn seed_comment(
    app: &PgPool,
    world: Uuid,
    post_id: Uuid,
    author: Uuid,
    body: Value,
    created_at: OffsetDateTime,
) -> Uuid {
    let id = new_id();
    let mut tx = world_tx(app, world).await.expect("world_tx");
    sqlx::query(
        "INSERT INTO comments (id, world_id, post_id, author_account, body, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(world)
    .bind(post_id)
    .bind(author)
    .bind(&body)
    .bind(created_at)
    .execute(&mut *tx)
    .await
    .expect("seed comment");
    tx.commit().await.expect("commit");
    id
}

async fn seed_hashtag(app: &PgPool, world: Uuid, tag: &str, post_id: Uuid) {
    seed_hashtag_in(app, world, APP, tag, post_id).await
}

async fn seed_hashtag_in(app: &PgPool, world: Uuid, app_id: &str, tag: &str, post_id: Uuid) {
    let mut tx = world_tx(app, world).await.expect("world_tx");
    sqlx::query("INSERT INTO hashtags (world_id, app_id, tag, post_id) VALUES ($1, $2, $3, $4)")
        .bind(world)
        .bind(app_id)
        .bind(tag)
        .bind(post_id)
        .execute(&mut *tx)
        .await
        .expect("seed hashtag");
    tx.commit().await.expect("commit");
}

/// Base instant + `secs` — distinct, ordered timestamps for deterministic keyset.
fn at(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000 + secs).expect("timestamp")
}

/// One authed HTTP GET through the real router → `(status, parsed body)`.
async fn get(state: &AppState, uri: &str, token: &str) -> (StatusCode, Value) {
    let res = opn_core::http::app_router(state.clone())
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

/// The `id` of every item in an `{ items, next_cursor }` page, in order.
fn item_ids(body: &Value) -> Vec<String> {
    body["items"]
        .as_array()
        .unwrap_or_else(|| panic!("no items array: {body}"))
        .iter()
        .map(|it| it["id"].as_str().expect("item id").to_string())
        .collect()
}

// ── tests ────────────────────────────────────────────────────────────────────

/// Home shows the caller's own posts plus posts by accounts they follow, and
/// nothing else, newest first — the fan-out-on-read core.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn home_fans_out_self_and_followed(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let state = test_state(app.clone(), test_config()).await;
    let _srv = spawn_server(state.clone()).await;

    let (viewer_tok, viewer) = mint_full(&app, tenant, world, "viewer").await;
    let me = login_account(&app, world, &viewer.identity, "viewer").await;
    let (_a_tok, a) = mint_full(&app, tenant, world, "alice").await;
    let alice = seed_account(&app, world, a.identity.character_id, "alice").await;
    let (_b_tok, b) = mint_full(&app, tenant, world, "bob").await;
    let bob = seed_account(&app, world, b.identity.character_id, "bob").await;

    seed_follow(&app, world, me, alice).await; // follow alice, NOT bob

    let mine = seed_post(&app, world, me, json!({ "text": "mine" }), at(3)).await;
    let alices = seed_post(&app, world, alice, json!({ "text": "alice" }), at(2)).await;
    let _bobs = seed_post(&app, world, bob, json!({ "text": "bob" }), at(1)).await;

    let (status, body) = get(&state, &format!("/v1/feed/home?app_id={APP}"), &viewer_tok).await;
    assert_eq!(status, StatusCode::OK, "home: {body}");
    // newest first: mine (t3), alice (t2); bob is unfollowed → absent.
    assert_eq!(item_ids(&body), vec![mine.to_string(), alices.to_string()]);
    // The post shape carries the denormalized counters + opaque body.
    let first = &body["items"][0];
    assert_eq!(first["author_account"], json!(me.to_string()));
    assert_eq!(first["like_count"], json!(0));
    assert_eq!(first["body"], json!({ "text": "mine" }));
}

/// Paging home with the cursor walks the whole set once — no duplicate, no skip,
/// strictly newest-first (the keyset property the cursor idiom guarantees).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn home_pagination_walks_all_no_dup_or_skip(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let state = test_state(app.clone(), test_config()).await;
    let _srv = spawn_server(state.clone()).await;

    let (tok, viewer) = mint_full(&app, tenant, world, "viewer").await;
    let me = login_account(&app, world, &viewer.identity, "viewer").await;
    let mut expected = Vec::new();
    for i in 0..5 {
        expected.push(
            seed_post(&app, world, me, json!({ "n": i }), at(i))
                .await
                .to_string(),
        );
    }
    expected.reverse(); // newest (i=4) first

    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..10 {
        let uri = match &cursor {
            Some(c) => format!("/v1/feed/home?app_id={APP}&limit=2&cursor={c}"),
            None => format!("/v1/feed/home?app_id={APP}&limit=2"),
        };
        let (status, body) = get(&state, &uri, &tok).await;
        assert_eq!(status, StatusCode::OK, "page: {body}");
        let ids = item_ids(&body);
        assert!(ids.len() <= 2, "limit honored");
        seen.extend(ids);
        match body["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    assert_eq!(seen, expected, "full set, in order, no dup/skip");
}

/// Home is *my* feed: a missing `app_id` is `invalid`, and a caller not logged
/// into the app is `forbidden` (the `active_account` gate).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn home_missing_app_id_invalid_and_not_logged_in_forbidden(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let state = test_state(app.clone(), test_config()).await;
    let _srv = spawn_server(state.clone()).await;

    let (tok, _who) = mint_full(&app, tenant, world, "nobody").await; // no login_account

    let (status, body) = get(&state, "/v1/feed/home", &tok).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "missing app_id: {body}");
    assert_eq!(body["code"], json!("invalid"));

    let (status, body) = get(&state, &format!("/v1/feed/home?app_id={APP}"), &tok).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "not logged in: {body}");
    assert_eq!(body["code"], json!("forbidden"));
}

/// Profile lists one author's posts, newest first; a non-member is `forbidden`;
/// an unknown author is an empty page (no existence probe).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn profile_lists_author_and_gates_membership(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let state = test_state(app.clone(), test_config()).await;
    let _srv = spawn_server(state.clone()).await;

    let (viewer_tok, viewer) = mint_full(&app, tenant, world, "viewer").await;
    login_account(&app, world, &viewer.identity, "viewer").await;
    let (_a_tok, a) = mint_full(&app, tenant, world, "alice").await;
    let alice = seed_account(&app, world, a.identity.character_id, "alice").await;

    let p2 = seed_post(&app, world, alice, json!({ "n": 2 }), at(2)).await;
    let p1 = seed_post(&app, world, alice, json!({ "n": 1 }), at(1)).await;

    let (status, body) = get(
        &state,
        &format!("/v1/feed/profile/{alice}?app_id={APP}"),
        &viewer_tok,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "profile: {body}");
    assert_eq!(item_ids(&body), vec![p2.to_string(), p1.to_string()]);

    // Unknown author → empty page, still 200.
    let (status, body) = get(
        &state,
        &format!("/v1/feed/profile/{}?app_id={APP}", new_id()),
        &viewer_tok,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(item_ids(&body).is_empty(), "unknown author empty: {body}");

    // A caller with no app account → forbidden.
    let (stranger_tok, _s) = mint_full(&app, tenant, world, "stranger").await;
    let (status, body) = get(
        &state,
        &format!("/v1/feed/profile/{alice}?app_id={APP}"),
        &stranger_tok,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "non-member: {body}");
    assert_eq!(body["code"], json!("forbidden"));
}

/// Post detail returns the post plus a newest-first, cursor-paged comment list.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn post_detail_returns_post_and_comments_paginated(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let state = test_state(app.clone(), test_config()).await;
    let _srv = spawn_server(state.clone()).await;

    let (tok, viewer) = mint_full(&app, tenant, world, "viewer").await;
    let me = login_account(&app, world, &viewer.identity, "viewer").await;
    let post = seed_post(&app, world, me, json!({ "text": "topic" }), at(10)).await;
    let c3 = seed_comment(&app, world, post, me, json!({ "t": "c3" }), at(3)).await;
    let c2 = seed_comment(&app, world, post, me, json!({ "t": "c2" }), at(2)).await;
    let c1 = seed_comment(&app, world, post, me, json!({ "t": "c1" }), at(1)).await;

    // First page: 2 newest comments + a cursor.
    let (status, body) = get(
        &state,
        &format!("/v1/feed/posts/{post}?app_id={APP}&limit=2"),
        &tok,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "detail: {body}");
    assert_eq!(body["post"]["id"], json!(post.to_string()));
    let comment_ids: Vec<String> = body["comments"]
        .as_array()
        .expect("comments")
        .iter()
        .map(|c| c["id"].as_str().expect("comment id").to_string())
        .collect();
    assert_eq!(comment_ids, vec![c3.to_string(), c2.to_string()]);
    let cursor = body["next_cursor"]
        .as_str()
        .expect("next cursor")
        .to_string();

    // Second page: the last comment, no further cursor.
    let (status, body) = get(
        &state,
        &format!("/v1/feed/posts/{post}?app_id={APP}&limit=2&cursor={cursor}"),
        &tok,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let comment_ids: Vec<String> = body["comments"]
        .as_array()
        .expect("comments")
        .iter()
        .map(|c| c["id"].as_str().expect("comment id").to_string())
        .collect();
    assert_eq!(comment_ids, vec![c1.to_string()]);
    assert!(body["next_cursor"].is_null(), "last page: {body}");
}

/// Detail on a missing post is `not_found`; a non-member gets `forbidden` even
/// for a real post (existence never leaks across the membership gate).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn post_detail_missing_not_found_and_non_member_forbidden(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let state = test_state(app.clone(), test_config()).await;
    let _srv = spawn_server(state.clone()).await;

    let (tok, viewer) = mint_full(&app, tenant, world, "viewer").await;
    let me = login_account(&app, world, &viewer.identity, "viewer").await;
    let post = seed_post(&app, world, me, json!({ "text": "real" }), at(1)).await;

    let (status, body) = get(
        &state,
        &format!("/v1/feed/posts/{}?app_id={APP}", new_id()),
        &tok,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "missing: {body}");
    assert_eq!(body["code"], json!("not_found"));

    let (stranger_tok, _s) = mint_full(&app, tenant, world, "stranger").await;
    let (status, body) = get(
        &state,
        &format!("/v1/feed/posts/{post}?app_id={APP}"),
        &stranger_tok,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "non-member real post: {body}"
    );
    assert_eq!(body["code"], json!("forbidden"));
}

/// Hashtag page returns the posts under a tag, newest first, and matches
/// case-insensitively (tags are stored lowercased).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn hashtag_page_lowercased_match(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let state = test_state(app.clone(), test_config()).await;
    let _srv = spawn_server(state.clone()).await;

    let (tok, viewer) = mint_full(&app, tenant, world, "viewer").await;
    let me = login_account(&app, world, &viewer.identity, "viewer").await;

    let tagged_new = seed_post(&app, world, me, json!({ "text": "#rustlang a" }), at(3)).await;
    let tagged_old = seed_post(&app, world, me, json!({ "text": "#rustlang b" }), at(2)).await;
    let other = seed_post(&app, world, me, json!({ "text": "#other" }), at(1)).await;
    seed_hashtag(&app, world, "rustlang", tagged_new).await;
    seed_hashtag(&app, world, "rustlang", tagged_old).await;
    seed_hashtag(&app, world, "other", other).await;

    // Uppercase query still matches the lowercased stored tag.
    let (status, body) = get(
        &state,
        &format!("/v1/feed/hashtags/RustLang?app_id={APP}"),
        &tok,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "hashtag: {body}");
    assert_eq!(
        item_ids(&body),
        vec![tagged_new.to_string(), tagged_old.to_string()],
        "only #rustlang posts, newest first"
    );
}

/// Every read is world-isolated: a second world with the same app never sees the
/// first world's posts (RLS on the read plane, closing the Sprint 9 audit early).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn reads_are_world_isolated(admin: PgPool) {
    let (world1, tenant1, _) = seed_world_tenant(&admin).await;
    let (world2, tenant2, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let state = test_state(app.clone(), test_config()).await;
    let _srv = spawn_server(state.clone()).await;

    // World 1: a viewer with a self-post.
    let (_t1, v1) = mint_full(&app, tenant1, world1, "w1viewer").await;
    let me1 = login_account(&app, world1, &v1.identity, "w1viewer").await;
    seed_post(&app, world1, me1, json!({ "text": "secret" }), at(1)).await;

    // World 2: its own viewer, logged into the same app slug.
    let (t2, v2) = mint_full(&app, tenant2, world2, "w2viewer").await;
    login_account(&app, world2, &v2.identity, "w2viewer").await;

    let (status, body) = get(&state, &format!("/v1/feed/home?app_id={APP}"), &t2).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        item_ids(&body).is_empty(),
        "world 2 sees none of world 1's posts: {body}"
    );
}

/// The two deferred part-A exit criteria: at 100 k posts the home query rides the
/// `posts_home` index (no `Seq Scan on posts`), and the p95 read is recorded.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn home_100k_uses_posts_home_index_and_records_p95(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let state = test_state(app.clone(), test_config()).await;

    let (_tok, viewer) = mint_full(&app, tenant, world, "viewer").await;
    let me = login_account(&app, world, &viewer.identity, "viewer").await;

    // Bulk-seed 100 k self-posts in one statement (RLS-scoped via world_tx).
    {
        let mut tx = world_tx(&app, world).await.expect("world_tx");
        sqlx::query(
            "INSERT INTO posts (id, world_id, app_id, author_account, body, created_at) \
             SELECT gen_random_uuid(), $1, $2, $3, '{}'::jsonb, now() - (g * interval '1 second') \
             FROM generate_series(1, 100000) AS g",
        )
        .bind(world)
        .bind(APP)
        .bind(me)
        .execute(&mut *tx)
        .await
        .expect("bulk seed");
        tx.commit().await.expect("commit");
    }
    // Fresh stats so the planner picks the index (owner runs ANALYZE; RLS-exempt).
    sqlx::query("ANALYZE posts")
        .execute(&admin)
        .await
        .expect("analyze");

    // EXPLAIN the EXACT query the endpoint runs — bind `EXPLAIN {read::HOME_SQL}`,
    // not a hand-copy, so a home-query regression to a seq scan can't hide behind
    // a stale duplicate (adversarial test-gap review, 2026-07-19).
    let plan: Vec<String> = {
        let mut tx = world_tx(&app, world).await.expect("world_tx");
        let rows: Vec<String> = sqlx::query_scalar(feed::read::HOME_SQL_EXPLAIN)
            .bind(world)
            .bind(APP)
            .bind(me)
            .bind(None::<OffsetDateTime>)
            .bind(Uuid::nil())
            .bind(51_i64)
            .fetch_all(&mut *tx)
            .await
            .expect("explain");
        tx.commit().await.expect("commit");
        rows
    };
    let plan = plan.join("\n");
    assert!(
        !plan.contains("Seq Scan on posts"),
        "home query must not seq-scan posts at 100k rows:\n{plan}"
    );
    assert!(
        plan.contains("posts_home"),
        "home query must ride the posts_home index:\n{plan}"
    );

    // p95 of the real read fn over the 100 k set (record it; §10.3 target < 10 ms
    // on dev hardware). Loose ceiling here so CI hardware variance never flakes —
    // the recorded number, not the gate, is the deliverable.
    let mut samples = Vec::new();
    for _ in 0..50 {
        let start = std::time::Instant::now();
        let page = feed::read::home(&state, &viewer.identity, APP, None, 50)
            .await
            .expect("home read");
        assert_eq!(page.items.len(), 50, "full page");
        samples.push(start.elapsed());
    }
    samples.sort();
    let p95 = samples[(samples.len() as f64 * 0.95) as usize - 1];
    println!("feed home p95 @ 100k posts = {p95:?}");
    assert!(
        p95 < std::time::Duration::from_millis(200),
        "home p95 {p95:?} — regressed far past the <10ms target"
    );
}

/// Every read is app-scoped: a member of app A, passing `app_id=A`, can never
/// reach app B's data by pointing at a B post/author/tag. Without this the
/// `AND app_id = $2` predicates are dead weight no other test observes
/// (adversarial test-gap review, 2026-07-19 — the sprint keeper).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn reads_are_app_isolated(admin: PgPool) {
    const OTHER: &str = "chatterbox";
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let state = test_state(app.clone(), test_config()).await;
    let _srv = spawn_server(state.clone()).await;

    // Caller is a member of APP only.
    let (tok, viewer) = mint_full(&app, tenant, world, "viewer").await;
    login_account(&app, world, &viewer.identity, "viewer").await;

    // A different app, with its own author, post, comment and hashtag.
    let (_o_tok, o) = mint_full(&app, tenant, world, "otheruser").await;
    let other_author =
        seed_account_in(&app, world, o.identity.character_id, OTHER, "otheruser").await;
    let other_post = seed_post_in(
        &app,
        world,
        OTHER,
        other_author,
        json!({ "text": "#shared secret" }),
        at(1),
    )
    .await;
    seed_hashtag_in(&app, world, OTHER, "shared", other_post).await;

    // Detail on a real post in app OTHER, asked as app APP → not_found (scoped
    // out, indistinguishable from missing — no cross-app existence oracle).
    let (status, _b) = get(
        &state,
        &format!("/v1/feed/posts/{other_post}?app_id={APP}"),
        &tok,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "cross-app post detail hidden"
    );

    // Profile of app OTHER's author, asked as app APP → empty page.
    let (status, body) = get(
        &state,
        &format!("/v1/feed/profile/{other_author}?app_id={APP}"),
        &tok,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        item_ids(&body).is_empty(),
        "cross-app profile empty: {body}"
    );

    // Hashtag app OTHER used, asked as app APP → empty (tag rows are app-scoped).
    let (status, body) = get(
        &state,
        &format!("/v1/feed/hashtags/shared?app_id={APP}"),
        &tok,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        item_ids(&body).is_empty(),
        "cross-app hashtag empty: {body}"
    );
}

/// post_detail returns only THIS post's comments — a second post's comment never
/// bleeds in (guards the `WHERE post_id = $1` scope, which a single-post test
/// leaves unobserved).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn post_detail_scopes_comments_to_post(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let state = test_state(app.clone(), test_config()).await;
    let _srv = spawn_server(state.clone()).await;

    let (tok, viewer) = mint_full(&app, tenant, world, "viewer").await;
    let me = login_account(&app, world, &viewer.identity, "viewer").await;
    let post_a = seed_post(&app, world, me, json!({ "text": "A" }), at(10)).await;
    let post_b = seed_post(&app, world, me, json!({ "text": "B" }), at(9)).await;
    let a_comment = seed_comment(&app, world, post_a, me, json!({ "t": "on-a" }), at(2)).await;
    let _b_comment = seed_comment(&app, world, post_b, me, json!({ "t": "on-b" }), at(1)).await;

    let (status, body) = get(
        &state,
        &format!("/v1/feed/posts/{post_a}?app_id={APP}"),
        &tok,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "detail: {body}");
    let ids: Vec<String> = body["comments"]
        .as_array()
        .expect("comments")
        .iter()
        .map(|c| c["id"].as_str().expect("id").to_string())
        .collect();
    assert_eq!(
        ids,
        vec![a_comment.to_string()],
        "only post A's comment: {body}"
    );
}

/// The denormalized counters surface on the correct fields with the correct
/// values — a swapped or zeroed `like_count`/`comment_count` fails (every other
/// test only ever sees the default 0/0).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn post_item_surfaces_counters(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let state = test_state(app.clone(), test_config()).await;
    let _srv = spawn_server(state.clone()).await;

    let (tok, viewer) = mint_full(&app, tenant, world, "viewer").await;
    let me = login_account(&app, world, &viewer.identity, "viewer").await;
    let post = seed_post(&app, world, me, json!({ "text": "popular" }), at(1)).await;
    set_counts(&app, world, post, 5, 3).await; // distinct so a field swap fails

    let (status, body) = get(&state, &format!("/v1/feed/home?app_id={APP}"), &tok).await;
    assert_eq!(status, StatusCode::OK, "home: {body}");
    let item = &body["items"][0];
    assert_eq!(item["like_count"], json!(5), "like_count surfaced: {item}");
    assert_eq!(
        item["comment_count"],
        json!(3),
        "comment_count surfaced: {item}"
    );
}

/// A tampered `?cursor` is `invalid` at the HTTP boundary (the `FeedQuery::parts`
/// → `fail_response` wiring; `cursor::decode`'s own unit tests don't exercise the
/// route). Never a 500 or a silently-swallowed first page.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn garbage_cursor_is_invalid(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let state = test_state(app.clone(), test_config()).await;
    let _srv = spawn_server(state.clone()).await;

    let (tok, viewer) = mint_full(&app, tenant, world, "viewer").await;
    login_account(&app, world, &viewer.identity, "viewer").await;

    // `%21%21%21` = "!!!", not valid base64url.
    let (status, body) = get(
        &state,
        &format!("/v1/feed/home?app_id={APP}&cursor=%21%21%21"),
        &tok,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "garbage cursor: {body}");
    assert_eq!(body["code"], json!("invalid"));
}

/// `limit` is clamped to [1, 100]: an oversize request caps at 100, and
/// `limit=0` still returns a page (floor ≥ 1), so dropping either clamp bound
/// fails.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn limit_clamped_to_100_and_floored(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let state = test_state(app.clone(), test_config()).await;
    let _srv = spawn_server(state.clone()).await;

    let (tok, viewer) = mint_full(&app, tenant, world, "viewer").await;
    let me = login_account(&app, world, &viewer.identity, "viewer").await;
    for i in 0..101 {
        seed_post(&app, world, me, json!({ "n": i }), at(i)).await;
    }

    let (status, body) = get(
        &state,
        &format!("/v1/feed/home?app_id={APP}&limit=1000000"),
        &tok,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(item_ids(&body).len(), 100, "oversize limit capped at 100");

    let (status, body) = get(&state, &format!("/v1/feed/home?app_id={APP}&limit=0"), &tok).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !item_ids(&body).is_empty(),
        "limit=0 floored to >=1, not empty"
    );
}

/// Follows are one-directional: A following B puts B's posts in A's home, but
/// does NOT put A's posts in B's home (a symmetric-follow bug would pass the
/// forward-only assertion in `home_fans_out_self_and_followed`).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn follows_are_directional(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let state = test_state(app.clone(), test_config()).await;
    let _srv = spawn_server(state.clone()).await;

    let (a_tok, a) = mint_full(&app, tenant, world, "aaa").await;
    let a_acct = login_account(&app, world, &a.identity, "aaa").await;
    let (b_tok, b) = mint_full(&app, tenant, world, "bbb").await;
    let b_acct = login_account(&app, world, &b.identity, "bbb").await;

    seed_follow(&app, world, a_acct, b_acct).await; // A → B only
    let a_post = seed_post(&app, world, a_acct, json!({ "text": "by a" }), at(2)).await;
    let b_post = seed_post(&app, world, b_acct, json!({ "text": "by b" }), at(1)).await;

    // A follows B → A's home has B's post (and A's own).
    let (_s, a_home) = get(&state, &format!("/v1/feed/home?app_id={APP}"), &a_tok).await;
    assert_eq!(
        item_ids(&a_home),
        vec![a_post.to_string(), b_post.to_string()],
        "A sees own + followed B"
    );

    // B does NOT follow A → B's home has only B's own post, not A's.
    let (_s, b_home) = get(&state, &format!("/v1/feed/home?app_id={APP}"), &b_tok).await;
    assert_eq!(
        item_ids(&b_home),
        vec![b_post.to_string()],
        "B sees only own, follow is one-way"
    );
}
