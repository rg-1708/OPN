//! Sprint 8 part A feed integration tests (OPN-CORE.md §10.3): the write plane
//! end to end — post + advisory fan-out, author-only cascade delete, like/unlike
//! counter exactness (incl. 32-way concurrency) with the durable author notify,
//! comment counts, follow/unfollow + authz, the not-logged-in `forbidden` gate,
//! and cross-world RLS. Reads (home/profile/detail/hashtag timelines) are part
//! B. Every test drives the real router over a live socket via `common::ws`.

mod common;

use std::time::Duration;

use common::ws::{connect_and_auth, mint_full, spawn_server, TestClient};
use common::{app_pool, seed_world_tenant, test_config, test_state};
use opn_core::infra::auth::Identity;
use opn_core::infra::db::world_tx;
use opn_core::infra::ids::new_id;
use opn_core::primitives::{feed, identity};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

const EVT_WAIT: Duration = Duration::from_secs(2);
const APP: &str = "instapic";

fn sub(topic: &str) -> Value {
    json!({ "cmd": "sub", "payload": { "topic": topic } })
}

/// Seed an app account for `who`'s character and set it active on the session
/// (what `feed` acts as). Returns the new account id.
async fn login_account(app: &PgPool, world: Uuid, who: &Identity, handle: &str) -> Uuid {
    let id = new_id();
    let mut tx = world_tx(app, world).await.expect("world_tx");
    sqlx::query(
        "INSERT INTO app_accounts (id, world_id, character_id, app_id, handle) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(world)
    .bind(who.character_id)
    .bind(APP)
    .bind(handle)
    .execute(&mut *tx)
    .await
    .expect("seed app account");
    tx.commit().await.expect("commit");
    identity::app_login(app, who, APP, id)
        .await
        .expect("app_login");
    id
}

async fn post(c: &mut TestClient, body: Value) -> Value {
    c.cmd(
        json!({ "cmd": "feed.post", "payload": { "app_id": APP, "body": body, "media_ids": [] } }),
    )
    .await
}

fn pid(ack: &Value) -> String {
    ack["payload"]["post_id"]
        .as_str()
        .unwrap_or_else(|| panic!("no post_id: {ack}"))
        .to_string()
}

async fn scalar_i64(app: &PgPool, world: Uuid, sql: &'static str, id: Uuid) -> i64 {
    let mut tx = world_tx(app, world).await.expect("world_tx");
    let n: i64 = sqlx::query_scalar(sql)
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .expect("scalar");
    tx.commit().await.expect("commit");
    n
}

async fn like_count(app: &PgPool, world: Uuid, post_id: Uuid) -> i64 {
    scalar_i64(
        app,
        world,
        "SELECT like_count::bigint FROM posts WHERE id = $1",
        post_id,
    )
    .await
}

// ── tests ────────────────────────────────────────────────────────────────────

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn post_creates_row_and_advises(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let srv = spawn_server(test_state(app.clone(), test_config()).await).await;

    let (author_tok, author) = mint_full(&app, tenant, world, "author").await;
    login_account(&app, world, &author.identity, "author").await;
    let (watch_tok, watcher) = mint_full(&app, tenant, world, "watcher").await;
    login_account(&app, world, &watcher.identity, "watcher").await;

    let mut a = connect_and_auth(srv.addr, &author_tok).await;
    let mut w = connect_and_auth(srv.addr, &watch_tok).await;
    // Watcher subscribes to the app feed.
    assert_eq!(w.cmd(sub(&format!("feed:{APP}"))).await["ok"], json!(true));

    let ack = post(&mut a, json!({ "text": "hello #World #world!" })).await;
    assert_eq!(ack["ok"], json!(true), "post ack: {ack}");
    let post_id = pid(&ack);

    // The advisory reaches the watcher.
    let evt = w.expect_evt(EVT_WAIT).await;
    assert_eq!(evt["evt"], json!("feed.activity"));
    assert_eq!(evt["payload"]["kind"], json!("post"));
    assert_eq!(evt["payload"]["post_id"], json!(post_id));

    // The row and its hashtag (deduped, lowercased) exist.
    let pid_u: Uuid = post_id.parse().expect("uuid");
    assert_eq!(
        scalar_i64(
            &app,
            world,
            "SELECT count(*) FROM posts WHERE id = $1",
            pid_u
        )
        .await,
        1
    );
    assert_eq!(
        scalar_i64(
            &app,
            world,
            "SELECT count(*) FROM hashtags WHERE post_id = $1",
            pid_u
        )
        .await,
        1,
        "#World and #world collapse to one"
    );
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn act_without_app_account_forbidden(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 4).await;
    let srv = spawn_server(test_state(app.clone(), test_config()).await).await;

    // Minted but never logged into the app.
    let (tok, _who) = mint_full(&app, tenant, world, "nobody").await;
    let mut c = connect_and_auth(srv.addr, &tok).await;

    let ack = post(&mut c, json!({ "text": "hi" })).await;
    assert_eq!(ack["ok"], json!(false));
    assert_eq!(ack["err"]["code"], json!("forbidden"), "post: {ack}");

    // Same gate on sub: no account for the app → forbidden.
    let s = c.cmd(sub(&format!("feed:{APP}"))).await;
    assert_eq!(s["err"]["code"], json!("forbidden"), "sub: {s}");
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn delete_cascades_children_author_only(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let srv = spawn_server(test_state(app.clone(), test_config()).await).await;

    let (a_tok, a) = mint_full(&app, tenant, world, "author").await;
    login_account(&app, world, &a.identity, "author").await;
    let (b_tok, b) = mint_full(&app, tenant, world, "other").await;
    login_account(&app, world, &b.identity, "other").await;

    let mut ac = connect_and_auth(srv.addr, &a_tok).await;
    let mut bc = connect_and_auth(srv.addr, &b_tok).await;

    let post_id = pid(&post(&mut ac, json!({ "text": "cascade me #tag" })).await);
    // A stranger likes + comments so there are children to cascade.
    assert_eq!(
        bc.cmd(json!({ "cmd": "feed.like", "payload": { "app_id": APP, "post_id": post_id } }))
            .await["ok"],
        json!(true)
    );
    assert_eq!(
        bc.cmd(json!({ "cmd": "feed.comment", "payload": { "app_id": APP, "post_id": post_id, "body": { "text": "nice" } } }))
            .await["ok"],
        json!(true)
    );

    // A non-author cannot delete it.
    let forbidden = bc
        .cmd(json!({ "cmd": "feed.delete", "payload": { "app_id": APP, "post_id": post_id } }))
        .await;
    assert_eq!(forbidden["err"]["code"], json!("forbidden"), "{forbidden}");

    // The author can; children cascade.
    let ok = ac
        .cmd(json!({ "cmd": "feed.delete", "payload": { "app_id": APP, "post_id": post_id } }))
        .await;
    assert_eq!(ok["ok"], json!(true), "{ok}");

    let pid_u: Uuid = post_id.parse().expect("uuid");
    for (label, sql) in [
        ("posts", "SELECT count(*) FROM posts WHERE id = $1"),
        ("likes", "SELECT count(*) FROM likes WHERE post_id = $1"),
        (
            "comments",
            "SELECT count(*) FROM comments WHERE post_id = $1",
        ),
        (
            "hashtags",
            "SELECT count(*) FROM hashtags WHERE post_id = $1",
        ),
    ] {
        assert_eq!(
            scalar_i64(&app, world, sql, pid_u).await,
            0,
            "{label} not cascaded"
        );
    }
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn like_counts_and_notifies_author(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let srv = spawn_server(test_state(app.clone(), test_config()).await).await;

    let (a_tok, a) = mint_full(&app, tenant, world, "author").await;
    login_account(&app, world, &a.identity, "author").await;
    let (b_tok, b) = mint_full(&app, tenant, world, "liker").await;
    login_account(&app, world, &b.identity, "liker").await;

    let mut ac = connect_and_auth(srv.addr, &a_tok).await;
    let mut bc = connect_and_auth(srv.addr, &b_tok).await;
    // Author listens on their own notify device topic.
    assert_eq!(
        ac.cmd(sub(&format!("notify:{}", a.identity.device_id)))
            .await["ok"],
        json!(true)
    );

    let post_id = pid(&post(&mut ac, json!({ "text": "like me" })).await);
    let pid_u: Uuid = post_id.parse().expect("uuid");

    // First like: count → 1, author gets a silent post_liked notify.
    assert_eq!(
        bc.cmd(json!({ "cmd": "feed.like", "payload": { "app_id": APP, "post_id": post_id } }))
            .await["ok"],
        json!(true)
    );
    assert_eq!(like_count(&app, world, pid_u).await, 1);
    let n = ac.expect_evt(EVT_WAIT).await;
    assert_eq!(n["evt"], json!("notify.event"));
    assert_eq!(n["payload"]["kind"], json!("post_liked"));
    assert_eq!(n["payload"]["class"], json!("silent"));
    assert_eq!(n["payload"]["payload"]["post_id"], json!(post_id));

    // Idempotent re-like: still one row, count unchanged, NO second notify.
    assert_eq!(
        bc.cmd(json!({ "cmd": "feed.like", "payload": { "app_id": APP, "post_id": post_id } }))
            .await["ok"],
        json!(true)
    );
    assert_eq!(
        like_count(&app, world, pid_u).await,
        1,
        "re-like must not double-count"
    );
    ac.expect_no_evt(Duration::from_millis(400)).await;
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn unlike_decrements_idempotent(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 6).await;
    let srv = spawn_server(test_state(app.clone(), test_config()).await).await;

    let (a_tok, a) = mint_full(&app, tenant, world, "author").await;
    login_account(&app, world, &a.identity, "author").await;
    let (b_tok, b) = mint_full(&app, tenant, world, "liker").await;
    login_account(&app, world, &b.identity, "liker").await;

    let mut ac = connect_and_auth(srv.addr, &a_tok).await;
    let mut bc = connect_and_auth(srv.addr, &b_tok).await;
    let post_id = pid(&post(&mut ac, json!({ "text": "x" })).await);
    let pid_u: Uuid = post_id.parse().expect("uuid");
    let like =
        |pid: &str| json!({ "cmd": "feed.like", "payload": { "app_id": APP, "post_id": pid } });
    let unlike =
        |pid: &str| json!({ "cmd": "feed.unlike", "payload": { "app_id": APP, "post_id": pid } });

    assert_eq!(bc.cmd(like(&post_id)).await["ok"], json!(true));
    assert_eq!(like_count(&app, world, pid_u).await, 1);
    assert_eq!(bc.cmd(unlike(&post_id)).await["ok"], json!(true));
    assert_eq!(like_count(&app, world, pid_u).await, 0);
    // Unlike again: no row, no underflow.
    assert_eq!(bc.cmd(unlike(&post_id)).await["ok"], json!(true));
    assert_eq!(
        like_count(&app, world, pid_u).await,
        0,
        "unlike must not go negative"
    );
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn comment_increments_count(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 6).await;
    let srv = spawn_server(test_state(app.clone(), test_config()).await).await;

    let (a_tok, a) = mint_full(&app, tenant, world, "author").await;
    login_account(&app, world, &a.identity, "author").await;
    let mut ac = connect_and_auth(srv.addr, &a_tok).await;
    let post_id = pid(&post(&mut ac, json!({ "text": "topic" })).await);
    let pid_u: Uuid = post_id.parse().expect("uuid");

    let ack = ac
        .cmd(json!({ "cmd": "feed.comment", "payload": { "app_id": APP, "post_id": post_id, "body": { "text": "first!" } } }))
        .await;
    assert_eq!(ack["ok"], json!(true), "{ack}");
    assert!(ack["payload"]["comment_id"].is_string(), "{ack}");
    assert_eq!(
        scalar_i64(
            &app,
            world,
            "SELECT comment_count::bigint FROM posts WHERE id = $1",
            pid_u
        )
        .await,
        1
    );
    assert_eq!(
        scalar_i64(
            &app,
            world,
            "SELECT count(*) FROM comments WHERE post_id = $1",
            pid_u
        )
        .await,
        1
    );
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn follow_unfollow_and_authz(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 6).await;
    let srv = spawn_server(test_state(app.clone(), test_config()).await).await;

    let (a_tok, a) = mint_full(&app, tenant, world, "follower").await;
    let me = login_account(&app, world, &a.identity, "me").await;
    let (_b_tok, b) = mint_full(&app, tenant, world, "followee").await;
    let them = login_account(&app, world, &b.identity, "them").await;

    let mut ac = connect_and_auth(srv.addr, &a_tok).await;
    let follow = |acct: Uuid| json!({ "cmd": "feed.follow", "payload": { "app_id": APP, "account_id": acct } });
    let unfollow = |acct: Uuid| json!({ "cmd": "feed.unfollow", "payload": { "app_id": APP, "account_id": acct } });

    // Self-follow → invalid.
    assert_eq!(ac.cmd(follow(me)).await["err"]["code"], json!("invalid"));
    // Unknown target → not_found.
    assert_eq!(
        ac.cmd(follow(new_id())).await["err"]["code"],
        json!("not_found")
    );
    // Real follow.
    assert_eq!(ac.cmd(follow(them)).await["ok"], json!(true));
    let edge = "SELECT count(*) FROM follows WHERE follower_account = $1 AND followee_account = $2";
    let mut tx = world_tx(&app, world).await.expect("tx");
    let n: i64 = sqlx::query_scalar(edge)
        .bind(me)
        .bind(them)
        .fetch_one(&mut *tx)
        .await
        .expect("n");
    tx.commit().await.expect("commit");
    assert_eq!(n, 1);
    // Unfollow removes it; repeat is a no-op.
    assert_eq!(ac.cmd(unfollow(them)).await["ok"], json!(true));
    assert_eq!(ac.cmd(unfollow(them)).await["ok"], json!(true));
    let mut tx = world_tx(&app, world).await.expect("tx");
    let n: i64 = sqlx::query_scalar(edge)
        .bind(me)
        .bind(them)
        .fetch_one(&mut *tx)
        .await
        .expect("n");
    tx.commit().await.expect("commit");
    assert_eq!(n, 0);
}

/// The counter-exactness invariant (roadmap test plan): 32 distinct accounts
/// like one post concurrently → `like_count == 32`. Drives `feed::like`
/// directly (32 WS clients would be pure overhead) against the shared state.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn concurrent_likes_count_exact(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 40).await;
    let state = test_state(app.clone(), test_config()).await;

    // 32 accounts, each on its own session; account[0] authors the post.
    let mut ids = Vec::new();
    for i in 0..32 {
        let (_t, m) = mint_full(&app, tenant, world, &format!("acct-{i}")).await;
        login_account(&app, world, &m.identity, &format!("h{i}")).await;
        ids.push(m.identity);
    }
    let ack = feed::post(&state, &ids[0], APP, &json!({ "text": "storm" }), &[])
        .await
        .expect("post");
    let post_id: Uuid = ack["post_id"]
        .as_str()
        .expect("post_id")
        .parse()
        .expect("uuid");

    let mut tasks = Vec::new();
    for who in ids {
        let state = state.clone();
        tasks.push(tokio::spawn(async move {
            feed::like(&state, &who, APP, post_id, true)
                .await
                .expect("like");
        }));
    }
    for t in tasks {
        t.await.expect("join");
    }
    assert_eq!(
        like_count(&app, world, post_id).await,
        32,
        "every distinct like counts once"
    );
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn cross_world_rls(admin: PgPool) {
    let (world_a, tenant_a, _) = seed_world_tenant(&admin).await;
    let (world_b, _tenant_b, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 6).await;
    let state = test_state(app.clone(), test_config()).await;

    let (_t, a) = mint_full(&app, tenant_a, world_a, "a").await;
    login_account(&app, world_a, &a.identity, "a").await;
    feed::post(
        &state,
        &a.identity,
        APP,
        &json!({ "text": "world A only" }),
        &[],
    )
    .await
    .expect("post");

    // World A sees its post; world B (RLS) sees nothing.
    async fn posts_in(app: &PgPool, w: Uuid) -> i64 {
        let mut tx = world_tx(app, w).await.expect("tx");
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM posts")
            .fetch_one(&mut *tx)
            .await
            .expect("count");
        tx.commit().await.expect("commit");
        n
    }
    assert_eq!(posts_in(&app, world_a).await, 1);
    assert_eq!(
        posts_in(&app, world_b).await,
        0,
        "cross-world post must be invisible"
    );
}

/// Regression guard for the adversarial-review keeper (2026-07-19): a delete
/// racing a like/comment on the same post must never surface as `internal` (the
/// pre-fix FK race) — each op is either ok or a clean `not_found`. The `FOR
/// UPDATE` on the post in all three handlers serializes them. 50 rounds.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn delete_like_race_never_internal(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 20).await;
    let state = test_state(app.clone(), test_config()).await;

    let (_t, author) = mint_full(&app, tenant, world, "author").await;
    login_account(&app, world, &author.identity, "author").await;
    let (_t2, liker) = mint_full(&app, tenant, world, "liker").await;
    login_account(&app, world, &liker.identity, "liker").await;

    for _ in 0..50 {
        let ack = feed::post(&state, &author.identity, APP, &json!({ "text": "x" }), &[])
            .await
            .expect("post");
        let post_id: Uuid = ack["post_id"].as_str().expect("pid").parse().expect("uuid");
        let (s1, s2) = (state.clone(), state.clone());
        let (aid, lid) = (author.identity.clone(), liker.identity.clone());
        let del = tokio::spawn(async move { feed::delete(&s1, &aid, APP, post_id).await });
        let lk = tokio::spawn(async move { feed::like(&s2, &lid, APP, post_id, true).await });
        let (dr, lr) = (del.await.expect("join"), lk.await.expect("join"));
        use opn_core::primitives::Fail;
        assert!(
            !matches!(dr, Err(Fail::Internal(_))),
            "delete internal: {dr:?}"
        );
        assert!(
            !matches!(lr, Err(Fail::Internal(_))),
            "like internal: {lr:?}"
        );
    }
}

/// The advisory `feed.activity` fires with the right `kind` + `actor` on ALL
/// THREE emit sites (post/like/comment) — the two write-back sites were
/// dead-code-untested (adversarial test-gap review, 2026-07-19). Also pins the
/// absences: an idempotent re-like and a delete emit no advisory.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn like_and_comment_advise_feed(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let srv = spawn_server(test_state(app.clone(), test_config()).await).await;

    let (act_tok, actor) = mint_full(&app, tenant, world, "actor").await;
    let acct = login_account(&app, world, &actor.identity, "actor").await;
    let (watch_tok, watcher) = mint_full(&app, tenant, world, "watcher").await;
    login_account(&app, world, &watcher.identity, "watcher").await;

    let mut a = connect_and_auth(srv.addr, &act_tok).await;
    let mut w = connect_and_auth(srv.addr, &watch_tok).await;
    assert_eq!(w.cmd(sub(&format!("feed:{APP}"))).await["ok"], json!(true));

    let post_id = pid(&post(&mut a, json!({ "text": "watch me" })).await);
    let like = json!({ "cmd": "feed.like", "payload": { "app_id": APP, "post_id": post_id } });
    assert_eq!(a.cmd(like.clone()).await["ok"], json!(true));
    assert_eq!(
        a.cmd(json!({ "cmd": "feed.comment", "payload": { "app_id": APP, "post_id": post_id, "body": { "text": "mine" } } }))
            .await["ok"],
        json!(true)
    );

    // The watcher sees post → like → comment in order, each with the right kind
    // and the acting account as `actor`.
    for kind in ["post", "like", "comment"] {
        let e = w.expect_evt(EVT_WAIT).await;
        assert_eq!(e["evt"], json!("feed.activity"), "{kind}: {e}");
        assert_eq!(e["payload"]["kind"], json!(kind), "{e}");
        assert_eq!(e["payload"]["post_id"], json!(post_id), "{e}");
        assert_eq!(e["payload"]["actor"], json!(acct.to_string()), "{e}");
    }

    // Idempotent re-like → no advisory; delete → no advisory (activity kinds are
    // post|like|comment only).
    assert_eq!(a.cmd(like).await["ok"], json!(true));
    assert_eq!(
        a.cmd(json!({ "cmd": "feed.delete", "payload": { "app_id": APP, "post_id": post_id } }))
            .await["ok"],
        json!(true)
    );
    w.expect_no_evt(Duration::from_millis(400)).await;
}

/// Like/comment on a missing or cross-app post → `not_found`. Guards the
/// `AND p.app_id = $2` scoping (dropping it would let a cross-app write-back
/// ship green) and the miss paths, none of which the happy-path tests hit.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn like_comment_missing_post_not_found(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 6).await;
    let srv = spawn_server(test_state(app.clone(), test_config()).await).await;

    let (tok, who) = mint_full(&app, tenant, world, "actor").await;
    login_account(&app, world, &who.identity, "actor").await;
    // A real post under APP so the cross-app probe targets a real id.
    let mut c = connect_and_auth(srv.addr, &tok).await;
    let post_id = pid(&post(&mut c, json!({ "text": "real" })).await);

    // Missing post id.
    let missing = new_id();
    assert_eq!(
        c.cmd(json!({ "cmd": "feed.like", "payload": { "app_id": APP, "post_id": missing } }))
            .await["err"]["code"],
        json!("not_found")
    );
    assert_eq!(
        c.cmd(json!({ "cmd": "feed.comment", "payload": { "app_id": APP, "post_id": missing, "body": { "text": "x" } } }))
            .await["err"]["code"],
        json!("not_found")
    );
    // Real post, wrong app → not_found (the app_id scope holds). The actor has no
    // account in "other", so this also can't 500 — it stops at active_account? No:
    // the actor IS logged into APP only, so `other` → forbidden first. Give the
    // actor an "other" account to reach the app_id-scope check itself.
    let mut tx = world_tx(&app, world).await.expect("tx");
    let other_acct = new_id();
    sqlx::query(
        "INSERT INTO app_accounts (id, world_id, character_id, app_id, handle) \
         VALUES ($1, $2, $3, 'other', 'o')",
    )
    .bind(other_acct)
    .bind(world)
    .bind(who.identity.character_id)
    .execute(&mut *tx)
    .await
    .expect("seed other account");
    tx.commit().await.expect("commit");
    identity::app_login(&app, &who.identity, "other", other_acct)
        .await
        .expect("login other");
    assert_eq!(
        c.cmd(json!({ "cmd": "feed.like", "payload": { "app_id": "other", "post_id": post_id } }))
            .await["err"]["code"],
        json!("not_found"),
        "a post in APP must be invisible under app_id=other"
    );
}

/// Self-like bumps the count but sends NO `post_liked` notify to yourself — a
/// removed `author_account != account` guard would otherwise self-notify, and no
/// other test exercises the equal case (test-gap review, 2026-07-19).
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn self_like_no_notify(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 6).await;
    let srv = spawn_server(test_state(app.clone(), test_config()).await).await;

    let (tok, who) = mint_full(&app, tenant, world, "solo").await;
    login_account(&app, world, &who.identity, "solo").await;
    let mut c = connect_and_auth(srv.addr, &tok).await;
    assert_eq!(
        c.cmd(sub(&format!("notify:{}", who.identity.device_id)))
            .await["ok"],
        json!(true)
    );
    let post_id = pid(&post(&mut c, json!({ "text": "mine alone" })).await);
    let pid_u: Uuid = post_id.parse().expect("uuid");

    assert_eq!(
        c.cmd(json!({ "cmd": "feed.like", "payload": { "app_id": APP, "post_id": post_id } }))
            .await["ok"],
        json!(true)
    );
    assert_eq!(
        like_count(&app, world, pid_u).await,
        1,
        "self-like still counts"
    );
    c.expect_no_evt(Duration::from_millis(400)).await;
}

/// The shared media gate (`media::assert_owned_live`, extracted this sprint) on
/// the post path: too many ids → `invalid`; a foreign id → `forbidden`; a
/// media-only post (`{}` body + one owned live media) → ok.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn post_media_validated(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 6).await;
    let srv = spawn_server(test_state(app.clone(), test_config()).await).await;

    let (tok, who) = mint_full(&app, tenant, world, "poster").await;
    login_account(&app, world, &who.identity, "poster").await;
    let mut c = connect_and_auth(srv.addr, &tok).await;

    let post_media = |body: Value, media: Vec<Uuid>| json!({ "cmd": "feed.post", "payload": { "app_id": APP, "body": body, "media_ids": media } });

    // > 8 media ids → invalid (rejected before any DB lookup).
    let many: Vec<Uuid> = (0..9).map(|_| new_id()).collect();
    assert_eq!(
        c.cmd(post_media(json!({ "text": "hi" }), many)).await["err"]["code"],
        json!("invalid")
    );
    // A media id the caller doesn't own → forbidden.
    assert_eq!(
        c.cmd(post_media(json!({ "text": "hi" }), vec![new_id()]))
            .await["err"]["code"],
        json!("forbidden")
    );
    // Seed a live media owned by the caller; a media-only post ({} body) is ok.
    let media_id = new_id();
    let mut tx = world_tx(&app, world).await.expect("tx");
    sqlx::query(
        "INSERT INTO media (id, world_id, owner_character, kind, mime, bytes, state, has_thumb) \
         VALUES ($1, $2, $3, 'photo', 'image/jpeg', 100, 'live', false)",
    )
    .bind(media_id)
    .bind(world)
    .bind(who.identity.character_id)
    .execute(&mut *tx)
    .await
    .expect("seed media");
    tx.commit().await.expect("commit");
    let ok = c.cmd(post_media(json!({}), vec![media_id])).await;
    assert_eq!(ok["ok"], json!(true), "media-only post: {ok}");
}
