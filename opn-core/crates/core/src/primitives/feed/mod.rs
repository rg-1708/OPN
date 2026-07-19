//! feed primitive (OPN-CORE.md §10.3): the social surface — posts, follows,
//! likes, comments, built to the fan-out-on-read design. No first-party app
//! consumes it in v1 (OPN.md §14.5: the primitive ships, the app is deferred).
//!
//! This module is Sprint 8 **part A**: the write plane + the advisory event +
//! sub authorization. The HTTP read surface (home/profile/detail/hashtag
//! timelines, cursor idiom, the 100 k-row EXPLAIN test) is part B — a genuine
//! seam (writes vs reads) on the committed part-A base, the same rhythm as
//! Sprints 4–7.
//!
//! Feed acts as **app accounts**, not characters: every post/like/comment/follow
//! is authored by the caller's *active* account for the named app (the session's
//! `app_accounts[app_id]`, set by `identity.app_login`). Not logged into the app
//! → `forbidden`. Sub authz is looser: any character owning *an* account for the
//! app may watch its `feed:<app>` advisory stream (§10.3).

use contracts::types::FeedActivityKind;
use contracts::{ErrCode, Evt, NotifyClass};
use serde_json::{json, Value};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use super::notify::{self, Notification};
use super::Fail;
use crate::infra::auth::Identity;
use crate::infra::db::world_tx;
use crate::infra::ids::new_id;
use crate::state::AppState;

/// Sprint 8 part B: the HTTP read surface (home/profile/detail/hashtag),
/// built on part A's committed schema + writes. Shares `active_account` and
/// `APP_ID_MAX` with this module (descendant-visible).
pub mod read;

/// Max serialized body for a post or comment (§10.3). Above this → `too_large`.
const BODY_MAX_BYTES: usize = 4 * 1024;
/// Max attachments per post — bounds the owned+live check; media is validated,
/// not merely counted.
const MEDIA_MAX: usize = 8;
/// Hashtag grammar (§10.3): `#[\p{Alnum}_]{1,32}`, ≤ 10 per post, lowercased.
const HASHTAG_MAX_LEN: usize = 32;
const HASHTAGS_PER_POST: usize = 10;
/// App-id slug cap — mirrors the `feed:<app>` topic cap (`topic.rs`) on the
/// write path so an oversize slug can't reach an indexed lookup (defense in
/// depth; `active_account` already short-circuits an unknown slug).
const APP_ID_MAX: usize = 64;

/// `feed.post` (§10.3): validate → media check → insert post + hashtags in one
/// tx → advise the feed. Author is the caller's active account for the app.
pub async fn post(
    state: &AppState,
    who: &Identity,
    app_id: &str,
    body: &Value,
    media_ids: &[Uuid],
) -> Result<Value, Fail> {
    validate_doc(body)?;
    if media_ids.len() > MEDIA_MAX {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    if !has_content(body) && media_ids.is_empty() {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    // Attachment gate, shared with channels (roadmap Sprint 8 item 2): every id
    // must be a live media owned by the caller. Its own tx, like channels.send.
    if !media_ids.is_empty() {
        super::media::assert_owned_live(state, who, media_ids).await?;
    }

    let tags = parse_hashtags(body);
    let post_id = new_id();
    let mut tx = world_tx(&state.pg, who.world_id).await?;
    let author = active_account(&mut tx, who.session_id, app_id).await?;
    sqlx::query(
        "INSERT INTO posts (id, world_id, app_id, author_account, body, media_ids) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(post_id)
    .bind(who.world_id)
    .bind(app_id)
    .bind(author)
    .bind(body)
    .bind(media_ids)
    .execute(&mut *tx)
    .await?;
    for tag in &tags {
        sqlx::query(
            "INSERT INTO hashtags (world_id, app_id, tag, post_id) VALUES ($1, $2, $3, $4) \
             ON CONFLICT DO NOTHING",
        )
        .bind(who.world_id)
        .bind(app_id)
        .bind(tag)
        .bind(post_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    activity(
        state,
        who.world_id,
        app_id,
        FeedActivityKind::Post,
        post_id,
        author,
    )
    .await;
    Ok(json!({ "post_id": post_id }))
}

/// `feed.delete` (§10.3): author-only hard delete, cascading likes/comments/
/// hashtags in one tx (explicit deletes, not FK cascade — keeps every child
/// removal inside the RLS-scoped tx). A foreign post → `forbidden`; a missing
/// one → `not_found`.
pub async fn delete(
    state: &AppState,
    who: &Identity,
    app_id: &str,
    post_id: Uuid,
) -> Result<(), Fail> {
    let mut tx = world_tx(&state.pg, who.world_id).await?;
    let author = active_account(&mut tx, who.session_id, app_id).await?;
    // Lock the post row FOR UPDATE (its author id is also the authz check): this
    // serializes the delete against a concurrent like/comment on the same post,
    // so the child-insert FK and this cascade can't race into an `internal`
    // (adversarial review, 2026-07-19). A missing row → not_found; someone
    // else's → forbidden.
    let owner: Option<Uuid> = sqlx::query_scalar(
        "SELECT author_account FROM posts WHERE id = $1 AND app_id = $2 FOR UPDATE",
    )
    .bind(post_id)
    .bind(app_id)
    .fetch_optional(&mut *tx)
    .await?;
    match owner {
        None => return Err(Fail::Code(ErrCode::NotFound)),
        Some(a) if a != author => return Err(Fail::Code(ErrCode::Forbidden)),
        Some(_) => {}
    }
    for sql in [
        "DELETE FROM likes WHERE post_id = $1",
        "DELETE FROM comments WHERE post_id = $1",
        "DELETE FROM hashtags WHERE post_id = $1",
        "DELETE FROM posts WHERE id = $1",
    ] {
        sqlx::query(sql).bind(post_id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// `feed.like` / `feed.unlike` (§10.3). Insert/delete keyed `(post_id, account)`,
/// bump `like_count` in the same tx and only on a real change (a repeat like or
/// absent unlike is a silent no-op). A fresh like advises the feed and silently
/// notifies the author (unless it's their own post).
pub async fn like(
    state: &AppState,
    who: &Identity,
    app_id: &str,
    post_id: Uuid,
    add: bool,
) -> Result<(), Fail> {
    let mut tx = world_tx(&state.pg, who.world_id).await?;
    let account = active_account(&mut tx, who.session_id, app_id).await?;
    // Post must exist in this app; grab the author's character for the notify.
    // `FOR UPDATE OF p` locks the post (not the joined account) so a concurrent
    // delete serializes behind us and the like insert can't FK-race (review
    // 2026-07-19); it's also the same per-post lock the count UPDATE would take,
    // so it adds no contention beyond what already serialized concurrent likers.
    let author: Option<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT p.author_account, a.character_id FROM posts p \
         JOIN app_accounts a ON a.id = p.author_account \
         WHERE p.id = $1 AND p.app_id = $2 FOR UPDATE OF p",
    )
    .bind(post_id)
    .bind(app_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((author_account, author_char)) = author else {
        return Err(Fail::Code(ErrCode::NotFound));
    };

    let changed = if add {
        let ins = sqlx::query(
            "INSERT INTO likes (world_id, post_id, account_id) VALUES ($1, $2, $3) \
             ON CONFLICT DO NOTHING",
        )
        .bind(who.world_id)
        .bind(post_id)
        .bind(account)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if ins == 1 {
            sqlx::query("UPDATE posts SET like_count = like_count + 1 WHERE id = $1")
                .bind(post_id)
                .execute(&mut *tx)
                .await?;
        }
        ins == 1
    } else {
        let del = sqlx::query("DELETE FROM likes WHERE post_id = $1 AND account_id = $2")
            .bind(post_id)
            .bind(account)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if del == 1 {
            sqlx::query("UPDATE posts SET like_count = like_count - 1 WHERE id = $1")
                .bind(post_id)
                .execute(&mut *tx)
                .await?;
        }
        del == 1
    };
    tx.commit().await?;

    if changed && add {
        activity(
            state,
            who.world_id,
            app_id,
            FeedActivityKind::Like,
            post_id,
            account,
        )
        .await;
        // Your-post-was-liked is a durable notify (§10.3), not the advisory
        // event; skip self-likes.
        if author_account != account {
            let n = Notification {
                app_id: app_id.to_string(),
                kind: "post_liked".into(),
                class: NotifyClass::Silent,
                payload: json!({ "post_id": post_id, "actor": account }),
            };
            if let Err(e) = notify::route(state, who.world_id, author_char, n, false).await {
                tracing::error!(error = ?e, %post_id, "feed like notify failed");
            }
        }
    }
    Ok(())
}

/// `feed.comment` (§10.3): insert a comment and bump `comment_count` in one tx,
/// then advise the feed. Ack `{ comment_id }`.
pub async fn comment(
    state: &AppState,
    who: &Identity,
    app_id: &str,
    post_id: Uuid,
    body: &Value,
) -> Result<Value, Fail> {
    validate_doc(body)?;
    if !has_content(body) {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    let comment_id = new_id();
    let mut tx = world_tx(&state.pg, who.world_id).await?;
    let author = active_account(&mut tx, who.session_id, app_id).await?;
    // FOR UPDATE serializes against a concurrent delete of this post so the
    // comment insert can't FK-race into an `internal` (review 2026-07-19).
    let exists: Option<i32> =
        sqlx::query_scalar("SELECT 1 FROM posts WHERE id = $1 AND app_id = $2 FOR UPDATE")
            .bind(post_id)
            .bind(app_id)
            .fetch_optional(&mut *tx)
            .await?;
    if exists.is_none() {
        return Err(Fail::Code(ErrCode::NotFound));
    }
    sqlx::query(
        "INSERT INTO comments (id, world_id, post_id, author_account, body) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(comment_id)
    .bind(who.world_id)
    .bind(post_id)
    .bind(author)
    .bind(body)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE posts SET comment_count = comment_count + 1 WHERE id = $1")
        .bind(post_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    activity(
        state,
        who.world_id,
        app_id,
        FeedActivityKind::Comment,
        post_id,
        author,
    )
    .await;
    Ok(json!({ "comment_id": comment_id }))
}

/// `feed.follow` / `feed.unfollow` (§10.3): idempotent edge insert/delete.
/// Self-follow → `invalid`; an unknown target account → `not_found`. No advisory
/// event (the design's activity kinds are post|like|comment).
pub async fn follow(
    state: &AppState,
    who: &Identity,
    app_id: &str,
    target: Uuid,
    add: bool,
) -> Result<(), Fail> {
    let mut tx = world_tx(&state.pg, who.world_id).await?;
    let follower = active_account(&mut tx, who.session_id, app_id).await?;
    if follower == target {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    let target_ok: Option<i32> =
        sqlx::query_scalar("SELECT 1 FROM app_accounts WHERE id = $1 AND app_id = $2")
            .bind(target)
            .bind(app_id)
            .fetch_optional(&mut *tx)
            .await?;
    if target_ok.is_none() {
        return Err(Fail::Code(ErrCode::NotFound));
    }
    if add {
        sqlx::query(
            "INSERT INTO follows (world_id, app_id, follower_account, followee_account) \
             VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
        )
        .bind(who.world_id)
        .bind(app_id)
        .bind(follower)
        .bind(target)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query(
            "DELETE FROM follows WHERE world_id = $1 AND app_id = $2 \
             AND follower_account = $3 AND followee_account = $4",
        )
        .bind(who.world_id)
        .bind(app_id)
        .bind(follower)
        .bind(target)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// `sub feed:<app>` authorization (§10.3): any character owning an app account
/// for the app may watch — the advisory stream leaks nothing a member can't
/// already read.
pub async fn authorize_sub(state: &AppState, who: &Identity, app_id: &str) -> Result<(), Fail> {
    let mut tx = world_tx(&state.pg, who.world_id).await?;
    let ok: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM app_accounts WHERE character_id = $1 AND app_id = $2 LIMIT 1",
    )
    .bind(who.character_id)
    .bind(app_id)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    if ok.is_some() {
        Ok(())
    } else {
        Err(Fail::Code(ErrCode::Forbidden))
    }
}

/// Publish the advisory `feed.activity` on `feed:<app>` (ephemeral fan-out).
async fn activity(
    state: &AppState,
    world: Uuid,
    app_id: &str,
    kind: FeedActivityKind,
    post_id: Uuid,
    actor: Uuid,
) {
    let evt = Evt::FeedActivity {
        app_id: app_id.to_string(),
        kind,
        post_id,
        actor,
    };
    crate::gateway::publish(state, world, &format!("feed:{app_id}"), &evt).await;
}

/// The caller's active app account for `app_id` — the id feed acts as. Stored on
/// the session row by `identity.app_login`; absent → the caller isn't logged
/// into the app, so `forbidden`. The session always exists (it's the caller's
/// own), so a parse failure is our-own corruption → `internal`.
async fn active_account(
    tx: &mut Transaction<'_, Postgres>,
    session_id: Uuid,
    app_id: &str,
) -> Result<Uuid, Fail> {
    if app_id.is_empty() || app_id.len() > APP_ID_MAX {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    let raw: Option<String> =
        sqlx::query_scalar("SELECT app_accounts ->> $1 FROM sessions WHERE id = $2")
            .bind(app_id)
            .bind(session_id)
            .fetch_one(&mut **tx)
            .await?;
    let raw = raw.ok_or(Fail::Code(ErrCode::Forbidden))?;
    raw.parse()
        .map_err(|_| Fail::Internal(anyhow::anyhow!("session active-account not a uuid")))
}

/// Size gate for an opaque body doc (§10.3): Core caps, never interprets. A JSON
/// `null` is rejected here — `posts.body`/`comments.body` are `jsonb NOT NULL`,
/// so a null-bodied post (which the content check waves through when media is
/// attached) would hit the DB constraint as an `internal`, not a clean
/// `invalid`. Reject it at the boundary; a media-only post sends `{}`.
fn validate_doc(body: &Value) -> Result<(), Fail> {
    if body.is_null() {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    let size = serde_json::to_vec(body)
        .map_err(|e| Fail::Internal(e.into()))?
        .len();
    if size > BODY_MAX_BYTES {
        return Err(Fail::Code(ErrCode::TooLarge));
    }
    Ok(())
}

/// A body carries content if it isn't JSON null and isn't the empty object — a
/// post/comment must say *something* (or, for a post, attach media).
fn has_content(body: &Value) -> bool {
    !body.is_null() && body.as_object().map(|o| !o.is_empty()).unwrap_or(true)
}

/// Parse hashtags from `body.text` (§10.3): `#` then a run of alphanumeric/`_`,
/// 1–32 chars, lowercased, de-duplicated in first-seen order, capped at 10.
/// Hand-written (no `regex` dep, matching the codebase's gif-host hand-parse):
/// `char::is_alphanumeric` is Unicode-aware, so it stands in for `\p{Alnum}`.
fn parse_hashtags(body: &Value) -> Vec<String> {
    let Some(text) = body.get("text").and_then(Value::as_str) else {
        return Vec::new();
    };
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '#' {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut j = start;
        while j < chars.len()
            && (chars[j].is_alphanumeric() || chars[j] == '_')
            && (j - start) < HASHTAG_MAX_LEN
        {
            j += 1;
        }
        if j > start {
            let tag: String = chars[start..j].iter().collect::<String>().to_lowercase();
            if !out.contains(&tag) {
                out.push(tag);
                if out.len() >= HASHTAGS_PER_POST {
                    break;
                }
            }
        }
        i = j.max(start); // always advance past the '#'
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashtags_parsed_lowercased_deduped_capped() {
        let body = json!({ "text": "Hello #World #world #RUST_lang no#leadingspace ##empty" });
        let tags = parse_hashtags(&body);
        // #World and #world collapse; #RUST_lang lowercases; the `#` after `no`
        // still counts (a hashtag needn't be space-preceded); `##empty` → "empty".
        assert_eq!(tags, vec!["world", "rust_lang", "leadingspace", "empty"]);
    }

    #[test]
    fn hashtag_length_capped_at_32() {
        let long = "a".repeat(40);
        let body = json!({ "text": format!("#{long}") });
        let tags = parse_hashtags(&body);
        assert_eq!(tags[0].len(), HASHTAG_MAX_LEN);
    }

    #[test]
    fn hashtags_capped_at_10() {
        let text = (0..15).map(|i| format!("#t{i} ")).collect::<String>();
        let tags = parse_hashtags(&json!({ "text": text }));
        assert_eq!(tags.len(), HASHTAGS_PER_POST);
    }

    #[test]
    fn no_text_no_hashtags() {
        assert!(parse_hashtags(&json!({ "photo": 1 })).is_empty());
        assert!(parse_hashtags(&json!("just a string")).is_empty());
    }

    #[test]
    fn content_and_size_checks() {
        assert!(has_content(&json!({ "text": "hi" })));
        assert!(has_content(&json!("x")));
        assert!(!has_content(&json!({})));
        assert!(!has_content(&Value::Null));
        assert!(validate_doc(&json!({ "text": "hi" })).is_ok());
        assert!(
            validate_doc(&json!({})).is_ok(),
            "empty object ok (media-only post)"
        );
        assert!(matches!(
            validate_doc(&json!({ "text": "x".repeat(5000) })),
            Err(Fail::Code(ErrCode::TooLarge))
        ));
        // A null body must be rejected cleanly — the column is jsonb NOT NULL,
        // so passing it through would 500 instead of acking `invalid`.
        assert!(matches!(
            validate_doc(&Value::Null),
            Err(Fail::Code(ErrCode::Invalid))
        ));
    }
}
