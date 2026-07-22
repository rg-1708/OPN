//! feed read plane (OPN-CORE.md §10.3, Sprint 8 **part B**): the fan-out-on-read
//! HTTP surface — home timeline, profile timeline, post detail + comments,
//! hashtag page. Every read is world-scoped by `world_tx` (RLS) and keysets on
//! the shared time cursor (CDR-7), newest first.
//!
//! Two authorization shapes, mirroring the write/sub split in `mod.rs`:
//!   * **Home** is *my* feed, so it acts as the caller's **active** account for
//!     the app (`active_account`) — not logged into the app → `forbidden`.
//!   * **Profile / detail / hashtag** don't act as a specific account; they gate
//!     on `require_app_member` (the caller owns *an* account for the app), the
//!     same looser rule `authorize_sub` uses. A non-member → `forbidden`, so a
//!     post's existence never leaks across apps.

use contracts::types::{CommentItem, PostItem};
use contracts::ErrCode;
use sqlx::{Postgres, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use super::{active_account, active_account_opt, APP_ID_MAX};
use crate::infra::auth::Identity;
use crate::infra::cursor::{self, Cursor, Page};
use crate::infra::db::world_tx;
use crate::infra::timefmt::rfc3339;
use crate::primitives::Fail;
use crate::state::AppState;

/// Page-size ceiling for every feed read (matches the ledger/media reads).
const LIMIT_MAX: i64 = 100;

/// The viewer-relative columns every post read appends (gap #6 + #8): the
/// author's public handle, plus whether the viewer account (`$V`) liked the post
/// and follows its author. Kept as scalar/correlated subqueries in the SELECT
/// list (never a FROM-clause JOIN) so the `posts_home` index still drives the
/// scan and the 100 k EXPLAIN gate holds — they evaluate per output row, after
/// LIMIT. `$V` is the viewer's app account (NULL ⇒ both flags false). Uses
/// `p.world_id`/`p.app_id` so the fragment is bind-position-independent across
/// the four queries; `concat!` keeps it a `&'static str`. Defined before
/// `home_select!` because `macro_rules!` resolves textually top-down.
macro_rules! viewer_cols {
    ($v:literal) => {
        concat!(
            "COALESCE((SELECT au.handle FROM app_accounts au WHERE au.id = p.author_account), '') AS author_handle, ",
            "EXISTS(SELECT 1 FROM likes l WHERE l.post_id = p.id AND l.account_id = ", $v, ") AS liked_by_viewer, ",
            "EXISTS(SELECT 1 FROM follows fv WHERE fv.world_id = p.world_id AND fv.app_id = p.app_id ",
            "AND fv.follower_account = ", $v, " AND fv.followee_account = p.author_account) AS author_following"
        )
    };
}

/// The home fan-out-on-read SELECT, as a macro so both the runtime query
/// ([`HOME_SQL`]) and the EXPLAIN gate ([`HOME_SQL_EXPLAIN`], used by
/// `tests/feed_read.rs`) derive from ONE literal — the plan test can never drift
/// from the live query (adversarial test-gap review, 2026-07-19). A `const` +
/// `concat!` keeps both `&'static str`, so no dynamic-SQL escape hatch is needed.
macro_rules! home_select {
    () => {
        concat!(
            "SELECT p.id, p.app_id, p.author_account, p.body, p.media_ids, ",
            "p.like_count, p.comment_count, p.created_at, ",
            // Viewer is the caller's active account, bound at $3 (same as the
            // home filter) — no extra bind, so the EXPLAIN test's 6 binds hold.
            viewer_cols!("$3"),
            " FROM posts p ",
            "WHERE p.world_id = $1 AND p.app_id = $2 ",
            "AND (p.author_account = $3 OR EXISTS ( ",
            "SELECT 1 FROM follows f ",
            "WHERE f.world_id = $1 AND f.app_id = $2 ",
            "AND f.follower_account = $3 AND f.followee_account = p.author_account)) ",
            "AND ($4::timestamptz IS NULL OR (p.created_at, p.id) < ($4, $5)) ",
            "ORDER BY p.created_at DESC, p.id DESC LIMIT $6"
        )
    };
}

/// Binds: `$1` world, `$2` app_id, `$3` the caller's active account, `$4` cursor
/// ts (NULL = first page), `$5` cursor id, `$6` limit + 1.
pub const HOME_SQL: &str = home_select!();
/// `EXPLAIN ` + [`HOME_SQL`] — the exact string the endpoint runs, for the 100 k
/// index gate. Same literal, prefixed; can't drift from `HOME_SQL`.
pub const HOME_SQL_EXPLAIN: &str = concat!("EXPLAIN ", home_select!());

#[derive(sqlx::FromRow)]
struct PostRow {
    id: Uuid,
    app_id: String,
    author_account: Uuid,
    author_handle: String,
    body: serde_json::Value,
    media_ids: Vec<Uuid>,
    like_count: i32,
    comment_count: i32,
    liked_by_viewer: bool,
    author_following: bool,
    created_at: OffsetDateTime,
}

impl From<PostRow> for PostItem {
    fn from(r: PostRow) -> Self {
        PostItem {
            id: r.id,
            app_id: r.app_id,
            author_account: r.author_account,
            author_handle: r.author_handle,
            body: r.body,
            media_ids: r.media_ids,
            like_count: r.like_count as i64,
            comment_count: r.comment_count as i64,
            liked_by_viewer: r.liked_by_viewer,
            author_following: r.author_following,
            created_at: rfc3339(r.created_at),
        }
    }
}

#[derive(sqlx::FromRow)]
struct CommentRow {
    id: Uuid,
    post_id: Uuid,
    author_account: Uuid,
    author_handle: String,
    body: serde_json::Value,
    created_at: OffsetDateTime,
}

impl From<CommentRow> for CommentItem {
    fn from(r: CommentRow) -> Self {
        CommentItem {
            id: r.id,
            post_id: r.post_id,
            author_account: r.author_account,
            author_handle: r.author_handle,
            body: r.body,
            created_at: rfc3339(r.created_at),
        }
    }
}

/// The caller must own an app account for `app_id` to read its feed (the same
/// gate `authorize_sub` applies to the advisory stream). Empty/oversize slug →
/// `invalid` (defense in depth, mirrors the write path).
async fn require_app_member(
    tx: &mut Transaction<'_, Postgres>,
    character: Uuid,
    app_id: &str,
) -> Result<(), Fail> {
    if app_id.is_empty() || app_id.len() > APP_ID_MAX {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    let ok: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM app_accounts WHERE character_id = $1 AND app_id = $2 LIMIT 1",
    )
    .bind(character)
    .bind(app_id)
    .fetch_optional(&mut **tx)
    .await?;
    ok.map(|_| ()).ok_or(Fail::Code(ErrCode::Forbidden))
}

/// Split a `Cursor` into the `(ts, id)` bind pair a keyset query wants, with the
/// no-cursor first-page case encoded as a NULL timestamp (the `$ts IS NULL`
/// branch short-circuits the comparison). Mirrors `ledger::store::history`.
fn cursor_binds(cursor: &Option<Cursor>) -> (Option<OffsetDateTime>, Uuid) {
    match cursor {
        Some(c) => (Some(c.ts), c.id),
        None => (None, Uuid::nil()),
    }
}

/// `GET /v1/feed/home` (§10.3, roadmap item 3): the fan-out-on-read home
/// timeline — posts the caller authored or whose author they follow, within the
/// app, newest first. The `EXISTS` form (not a JOIN) needs no dedup and rides
/// the `posts_home (world_id, app_id, created_at DESC, id DESC)` index (part B's
/// EXPLAIN test gates on it). Acts as the caller's active account.
pub async fn home(
    state: &AppState,
    who: &Identity,
    app_id: &str,
    cursor: Option<Cursor>,
    limit: i64,
) -> Result<Page<PostItem>, Fail> {
    let limit = limit.clamp(1, LIMIT_MAX) as usize;
    let (cur_ts, cur_id) = cursor_binds(&cursor);
    let mut tx = world_tx(&state.pg, who.world_id).await?;
    let me = active_account(&mut tx, who.session_id, app_id).await?;
    let rows: Vec<PostRow> = sqlx::query_as(HOME_SQL)
        .bind(who.world_id)
        .bind(app_id)
        .bind(me)
        .bind(cur_ts)
        .bind(cur_id)
        .bind(limit as i64 + 1)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(map_posts(rows, limit))
}

/// `GET /v1/feed/profile/:account` (§10.3): one author's posts, newest first, on
/// the `posts_author` index. A nonexistent author just yields an empty page (no
/// existence probe). App-member gated.
pub async fn profile(
    state: &AppState,
    who: &Identity,
    app_id: &str,
    author: Uuid,
    cursor: Option<Cursor>,
    limit: i64,
) -> Result<Page<PostItem>, Fail> {
    let limit = limit.clamp(1, LIMIT_MAX) as usize;
    let (cur_ts, cur_id) = cursor_binds(&cursor);
    let mut tx = world_tx(&state.pg, who.world_id).await?;
    require_app_member(&mut tx, who.character_id, app_id).await?;
    // Viewer account (if logged in) for the liked/following flags — $7, appended
    // after the six positional binds this query already uses (gap #6).
    let viewer = active_account_opt(&mut tx, who.session_id, app_id).await?;
    let rows: Vec<PostRow> = sqlx::query_as(concat!(
        "SELECT p.id, p.app_id, p.author_account, p.body, p.media_ids, ",
        "p.like_count, p.comment_count, p.created_at, ",
        viewer_cols!("$7"),
        " FROM posts p ",
        "WHERE p.world_id = $1 AND p.app_id = $2 AND p.author_account = $3 ",
        "AND ($4::timestamptz IS NULL OR (p.created_at, p.id) < ($4, $5)) ",
        "ORDER BY p.created_at DESC, p.id DESC LIMIT $6"
    ))
    .bind(who.world_id)
    .bind(app_id)
    .bind(author)
    .bind(cur_ts)
    .bind(cur_id)
    .bind(limit as i64 + 1)
    .bind(viewer)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(map_posts(rows, limit))
}

/// `GET /v1/feed/posts/:id` (§10.3): the post plus a cursor page of its comments
/// (newest first, `comments_post` index). App-member gated *before* the post
/// lookup, so a non-member can't distinguish a hidden post from a missing one.
/// Missing/wrong-app post → `not_found`.
pub async fn post_detail(
    state: &AppState,
    who: &Identity,
    app_id: &str,
    post_id: Uuid,
    cursor: Option<Cursor>,
    limit: i64,
) -> Result<(PostItem, Page<CommentItem>), Fail> {
    let limit = limit.clamp(1, LIMIT_MAX) as usize;
    let (cur_ts, cur_id) = cursor_binds(&cursor);
    let mut tx = world_tx(&state.pg, who.world_id).await?;
    require_app_member(&mut tx, who.character_id, app_id).await?;
    let viewer = active_account_opt(&mut tx, who.session_id, app_id).await?;
    let post: Option<PostRow> = sqlx::query_as(concat!(
        "SELECT p.id, p.app_id, p.author_account, p.body, p.media_ids, ",
        "p.like_count, p.comment_count, p.created_at, ",
        viewer_cols!("$3"),
        " FROM posts p WHERE p.id = $1 AND p.app_id = $2"
    ))
    .bind(post_id)
    .bind(app_id)
    .bind(viewer)
    .fetch_optional(&mut *tx)
    .await?;
    let post: PostItem = post.ok_or(Fail::Code(ErrCode::NotFound))?.into();
    let rows: Vec<CommentRow> = sqlx::query_as(
        "SELECT c.id, c.post_id, c.author_account, \
                COALESCE((SELECT au.handle FROM app_accounts au WHERE au.id = c.author_account), '') AS author_handle, \
                c.body, c.created_at FROM comments c \
         WHERE c.post_id = $1 \
           AND ($2::timestamptz IS NULL OR (c.created_at, c.id) < ($2, $3)) \
         ORDER BY c.created_at DESC, c.id DESC LIMIT $4",
    )
    .bind(post_id)
    .bind(cur_ts)
    .bind(cur_id)
    .bind(limit as i64 + 1)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let comments = cursor::page(rows, limit, |r| (r.created_at, r.id));
    Ok((
        post,
        Page {
            items: comments.items.into_iter().map(Into::into).collect(),
            next_cursor: comments.next_cursor,
        },
    ))
}

/// `GET /v1/feed/hashtags/:tag` (§10.3): posts carrying `tag`, newest first. The
/// `hashtags` PK `(world, app, tag, post_id)` selects the matching ids; the join
/// to `posts` (PK) orders them by time. Tag is lowercased to match write-time
/// storage. App-member gated. Bounded per tag, so no perf gate (unlike home).
pub async fn hashtag(
    state: &AppState,
    who: &Identity,
    app_id: &str,
    tag: &str,
    cursor: Option<Cursor>,
    limit: i64,
) -> Result<Page<PostItem>, Fail> {
    let limit = limit.clamp(1, LIMIT_MAX) as usize;
    let tag = tag.to_lowercase();
    let (cur_ts, cur_id) = cursor_binds(&cursor);
    let mut tx = world_tx(&state.pg, who.world_id).await?;
    require_app_member(&mut tx, who.character_id, app_id).await?;
    let viewer = active_account_opt(&mut tx, who.session_id, app_id).await?;
    let rows: Vec<PostRow> = sqlx::query_as(concat!(
        "SELECT p.id, p.app_id, p.author_account, p.body, p.media_ids, ",
        "p.like_count, p.comment_count, p.created_at, ",
        viewer_cols!("$7"),
        " FROM hashtags h JOIN posts p ON p.id = h.post_id ",
        "WHERE h.world_id = $1 AND h.app_id = $2 AND h.tag = $3 ",
        "AND ($4::timestamptz IS NULL OR (p.created_at, p.id) < ($4, $5)) ",
        "ORDER BY p.created_at DESC, p.id DESC LIMIT $6"
    ))
    .bind(who.world_id)
    .bind(app_id)
    .bind(&tag)
    .bind(cur_ts)
    .bind(cur_id)
    .bind(limit as i64 + 1)
    .bind(viewer)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(map_posts(rows, limit))
}

/// Overfetch → page of `PostItem`, cursor from the last kept row's `(created_at,
/// id)` — the one place the post read plane turns rows into a page.
fn map_posts(rows: Vec<PostRow>, limit: usize) -> Page<PostItem> {
    let paged = cursor::page(rows, limit, |r| (r.created_at, r.id));
    Page {
        items: paged.items.into_iter().map(Into::into).collect(),
        next_cursor: paged.next_cursor,
    }
}
