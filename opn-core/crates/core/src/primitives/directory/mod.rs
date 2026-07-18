//! directory primitive (OPN-CORE.md §10.7): the number→character choke point,
//! plus contacts, blocks, and listings. Nothing outside `resolve` reads
//! `characters.number` to route, so future virtual/burner numbers slot in
//! behind it without touching callers — and `resolve` is where a blocked pair
//! becomes indistinguishable from an unknown number (privacy).
//!
//! SQL lives inline here (media-style): the surface is small and every query is
//! world-scoped by the caller's `world_tx` (RLS). Directory has no events — it
//! is all request/response.

use contracts::types::{ContactItem, ListingItem, ResolveResult};
use contracts::ErrCode;
use sqlx::{Postgres, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use super::Fail;
use crate::infra::auth::Identity;
use crate::infra::cursor::{self, Cursor, Page};
use crate::infra::db::world_tx;
use crate::infra::ids::new_id;
use crate::infra::timefmt::rfc3339;
use crate::state::AppState;

/// Free-form field caps (§10.7). Numbers are short; display/title bounded so a
/// listing page or contact book stays cheap to ship.
const NUMBER_MAX: usize = 32;
const DISPLAY_MAX: usize = 128;
const APP_ID_MAX: usize = 64;
const KIND_MAX: usize = 32;
const TITLE_MAX: usize = 200;
/// Serialized `meta`/`body` bag cap.
const JSON_MAX: usize = 4 * 1024;
/// Upper bound on a listing TTL (1 year). Above this → `invalid`; a longer-lived
/// posting uses no TTL (never expires) instead. Also keeps `now() + interval`
/// well clear of the timestamptz overflow that a huge `ttl_secs` would hit.
const LISTING_TTL_MAX_SECS: i64 = 365 * 24 * 60 * 60;

/// Resolve a phone number to the character that holds it, within the caller's
/// world (RLS scopes the read). `None` = no such number **or** the pair is
/// blocked in either direction — the two are deliberately indistinguishable so a
/// block cannot be probed (§10.7).
///
/// Runs inside a caller-supplied `world_tx` so it composes into the
/// open_direct / calls.start transactions. `caller` is needed for the block
/// check: `caller` must not have blocked the target number, and the target must
/// not have blocked `caller`'s own number.
pub async fn resolve(
    tx: &mut Transaction<'_, Postgres>,
    caller: Uuid,
    number: &str,
) -> sqlx::Result<Option<Uuid>> {
    // Cap the untrusted number before the indexed lookup — an over-long string
    // is no valid number, so it is simply unreachable (None). One guard here
    // protects every resolve caller (open_direct, resolve_public, calls.start).
    if number.is_empty() || number.len() > NUMBER_MAX {
        return Ok(None);
    }
    sqlx::query_scalar(
        "SELECT c.id FROM characters c \
         WHERE c.number = $2 \
           AND NOT EXISTS ( \
               SELECT 1 FROM blocks b \
               WHERE b.blocker_character = $1 AND b.blocked_number = $2) \
           AND NOT EXISTS ( \
               SELECT 1 FROM blocks b JOIN characters me ON me.id = $1 \
               WHERE b.blocker_character = c.id AND b.blocked_number = me.number)",
    )
    .bind(caller)
    .bind(number)
    .fetch_optional(&mut **tx)
    .await
}

// ── contacts ────────────────────────────────────────────────────────────────

/// `directory.contact_upsert` (§10.7): create-or-replace the caller's contact
/// for `number`. An `avatar_media`, if given, must be a live media owned by the
/// caller — reuses the same ownership gate as message attachments.
pub async fn contact_upsert(
    state: &AppState,
    who: &Identity,
    number: &str,
    display_name: &str,
    avatar_media: Option<Uuid>,
    meta: Option<serde_json::Value>,
) -> Result<(), Fail> {
    if number.is_empty() || number.len() > NUMBER_MAX {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    if display_name.is_empty() || display_name.len() > DISPLAY_MAX {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    let meta = meta.unwrap_or_else(|| serde_json::json!({}));
    if serde_json::to_vec(&meta)
        .map(|v| v.len())
        .unwrap_or(usize::MAX)
        > JSON_MAX
    {
        return Err(Fail::Code(ErrCode::TooLarge));
    }
    // Owned-live avatar (§10.7). `all_owned_live` opens its own world_tx.
    if let Some(id) = avatar_media {
        if !super::media::all_owned_live(state, who, &[id]).await? {
            return Err(Fail::Code(ErrCode::Invalid));
        }
    }

    let mut tx = world_tx(&state.pg, who.world_id).await?;
    sqlx::query(
        "INSERT INTO contacts (owner_character, world_id, number, display_name, avatar_media, meta) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (owner_character, number) DO UPDATE \
           SET display_name = EXCLUDED.display_name, \
               avatar_media = EXCLUDED.avatar_media, \
               meta = EXCLUDED.meta",
    )
    .bind(who.character_id)
    .bind(who.world_id)
    .bind(number)
    .bind(display_name)
    .bind(avatar_media)
    .bind(&meta)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// `directory.contact_delete` (§10.7): drop the caller's contact for `number`.
/// A missing contact is a silent no-op (idempotent).
pub async fn contact_delete(state: &AppState, who: &Identity, number: &str) -> Result<(), Fail> {
    let mut tx = world_tx(&state.pg, who.world_id).await?;
    sqlx::query("DELETE FROM contacts WHERE owner_character = $1 AND number = $2")
        .bind(who.character_id)
        .bind(number)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct ContactRow {
    number: String,
    display_name: String,
    avatar_media: Option<Uuid>,
    meta: serde_json::Value,
    created_at: OffsetDateTime,
}

/// `directory.contacts` (§10.7): the caller's whole contact book, alphabetical.
/// Unpaginated — one character's contacts are a naturally bounded set.
// ponytail: no cursor. A contact book is small and per-character; add paging
// only if a real user's list gets large enough to matter.
pub async fn contacts(state: &AppState, who: &Identity) -> Result<Vec<ContactItem>, Fail> {
    let mut tx = world_tx(&state.pg, who.world_id).await?;
    let rows: Vec<ContactRow> = sqlx::query_as(
        "SELECT number, display_name, avatar_media, meta, created_at FROM contacts \
         WHERE owner_character = $1 ORDER BY display_name, number",
    )
    .bind(who.character_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows
        .into_iter()
        .map(|r| ContactItem {
            number: r.number,
            display_name: r.display_name,
            avatar_media: r.avatar_media,
            meta: r.meta,
            created_at: rfc3339(r.created_at),
        })
        .collect())
}

// ── blocks ──────────────────────────────────────────────────────────────────

/// `directory.block` (§10.7): block a number. Idempotent (`ON CONFLICT DO
/// NOTHING`). The number is free-form — you may block one that isn't a
/// character yet.
pub async fn block(state: &AppState, who: &Identity, number: &str) -> Result<(), Fail> {
    if number.is_empty() || number.len() > NUMBER_MAX {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    let mut tx = world_tx(&state.pg, who.world_id).await?;
    sqlx::query(
        "INSERT INTO blocks (blocker_character, world_id, blocked_number) \
         VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
    )
    .bind(who.character_id)
    .bind(who.world_id)
    .bind(number)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// `directory.unblock` (§10.7): remove a block (idempotent no-op if absent).
pub async fn unblock(state: &AppState, who: &Identity, number: &str) -> Result<(), Fail> {
    let mut tx = world_tx(&state.pg, who.world_id).await?;
    sqlx::query("DELETE FROM blocks WHERE blocker_character = $1 AND blocked_number = $2")
        .bind(who.character_id)
        .bind(number)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// `directory.blocks` (§10.7): the caller's blocked numbers, for an unblock UI.
pub async fn blocks(state: &AppState, who: &Identity) -> Result<Vec<String>, Fail> {
    let mut tx = world_tx(&state.pg, who.world_id).await?;
    let nums: Vec<String> = sqlx::query_scalar(
        "SELECT blocked_number FROM blocks WHERE blocker_character = $1 ORDER BY created_at DESC",
    )
    .bind(who.character_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(nums)
}

// ── resolve (public) ──────────────────────────────────────────────────────────

/// `directory.resolve` (§10.7): opaque routing. Returns only `reachable` plus
/// the caller's own saved label — never a character id. `reachable` is false for
/// both unknown and blocked numbers (privacy: a block must not be probeable).
pub async fn resolve_public(
    state: &AppState,
    who: &Identity,
    number: &str,
) -> Result<ResolveResult, Fail> {
    // Cap the untrusted number before either indexed lookup runs (the shared
    // `resolve` caps too, but this also short-circuits the display_name query).
    if number.is_empty() || number.len() > NUMBER_MAX {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    let mut tx = world_tx(&state.pg, who.world_id).await?;
    let reachable = resolve(&mut tx, who.character_id, number).await?.is_some();
    // The caller's OWN contact label — their data, so it leaks nothing about the
    // target and is present/absent independent of whether the number is real.
    let display_name: Option<String> = sqlx::query_scalar(
        "SELECT display_name FROM contacts WHERE owner_character = $1 AND number = $2",
    )
    .bind(who.character_id)
    .bind(number)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(ResolveResult {
        reachable,
        number: number.to_string(),
        display_name,
    })
}

// ── listings ──────────────────────────────────────────────────────────────────

/// `directory.listing_create` (§10.7): post a listing under an app, optional
/// TTL. Returns the new id.
#[allow(clippy::too_many_arguments)]
pub async fn listing_create(
    state: &AppState,
    who: &Identity,
    app_id: &str,
    kind: &str,
    title: &str,
    body: Option<serde_json::Value>,
    contact_number: &str,
    ttl_secs: Option<i64>,
) -> Result<serde_json::Value, Fail> {
    if app_id.is_empty() || app_id.len() > APP_ID_MAX {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    if kind.is_empty() || kind.len() > KIND_MAX {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    if title.is_empty() || title.len() > TITLE_MAX {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    if contact_number.is_empty() || contact_number.len() > NUMBER_MAX {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    if ttl_secs.is_some_and(|t| t <= 0 || t > LISTING_TTL_MAX_SECS) {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    let body = body.unwrap_or_else(|| serde_json::json!({}));
    if serde_json::to_vec(&body)
        .map(|v| v.len())
        .unwrap_or(usize::MAX)
        > JSON_MAX
    {
        return Err(Fail::Code(ErrCode::TooLarge));
    }

    let id = new_id();
    let mut tx = world_tx(&state.pg, who.world_id).await?;
    sqlx::query(
        "INSERT INTO listings \
           (id, world_id, owner_character, app_id, kind, title, body, contact_number, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, \
                 CASE WHEN $9::bigint IS NULL THEN NULL \
                      ELSE now() + make_interval(secs => $9) END)",
    )
    .bind(id)
    .bind(who.world_id)
    .bind(who.character_id)
    .bind(app_id)
    .bind(kind)
    .bind(title)
    .bind(&body)
    .bind(contact_number)
    .bind(ttl_secs)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(serde_json::json!({ "id": id }))
}

/// `directory.listing_delete` (§10.7): delete the caller's own listing. A row
/// that isn't the caller's (or doesn't exist / RLS-hidden) → `not_found` — no
/// existence leak of another character's listing.
pub async fn listing_delete(state: &AppState, who: &Identity, id: Uuid) -> Result<(), Fail> {
    let mut tx = world_tx(&state.pg, who.world_id).await?;
    let deleted = sqlx::query("DELETE FROM listings WHERE id = $1 AND owner_character = $2")
        .bind(id)
        .bind(who.character_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    if deleted == 0 {
        return Err(Fail::Code(ErrCode::NotFound));
    }
    Ok(())
}

#[derive(sqlx::FromRow)]
struct ListingRow {
    id: Uuid,
    app_id: String,
    kind: String,
    title: String,
    body: serde_json::Value,
    contact_number: String,
    created_at: OffsetDateTime,
    expires_at: Option<OffsetDateTime>,
}

/// `directory.listings` (§10.7): a page of *active* (unexpired) listings for an
/// app, newest-first on the shared cursor idiom (CDR-7). Expired rows are hidden
/// at read time even before the janitor deletes them.
pub async fn listings(
    state: &AppState,
    who: &Identity,
    app_id: &str,
    cursor: Option<Cursor>,
    limit: i64,
) -> Result<Page<ListingItem>, Fail> {
    if app_id.is_empty() || app_id.len() > APP_ID_MAX {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    let limit = limit.clamp(1, 100) as usize;
    let (cur_ts, cur_id) = match &cursor {
        Some(c) => (Some(c.ts), c.id),
        None => (None, Uuid::nil()),
    };
    let mut tx = world_tx(&state.pg, who.world_id).await?;
    let rows: Vec<ListingRow> = sqlx::query_as(
        "SELECT id, app_id, kind, title, body, contact_number, created_at, expires_at \
         FROM listings \
         WHERE app_id = $1 AND (expires_at IS NULL OR expires_at > now()) \
           AND ($2::timestamptz IS NULL OR (created_at, id) < ($2, $3)) \
         ORDER BY created_at DESC, id DESC LIMIT $4",
    )
    .bind(app_id)
    .bind(cur_ts)
    .bind(cur_id)
    .bind(limit as i64 + 1)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    let paged = cursor::page(rows, limit, |r| (r.created_at, r.id));
    Ok(Page {
        items: paged
            .items
            .into_iter()
            .map(|r| ListingItem {
                id: r.id,
                app_id: r.app_id,
                kind: r.kind,
                title: r.title,
                body: r.body,
                contact_number: r.contact_number,
                created_at: rfc3339(r.created_at),
                expires_at: r.expires_at.map(rfc3339),
            })
            .collect(),
        next_cursor: paged.next_cursor,
    })
}
