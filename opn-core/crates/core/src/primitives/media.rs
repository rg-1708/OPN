//! media primitive (OPN-CORE.md §10.6): presigned uploads with a pending→live
//! lifecycle. Core issues POST policies (caps enforced by MinIO), records rows,
//! and never proxies bytes; the janitor verifies live objects out of band and
//! reverts any that cheated the cap or never arrived.

use contracts::types::{MediaItem, MediaKind, UploadTarget, UploadTicket};
use contracts::ErrCode;
use futures_util::stream::{self, StreamExt};
use metrics::counter;
use time::OffsetDateTime;
use uuid::Uuid;

use super::Fail;
use crate::infra::auth::Identity;
use crate::infra::cursor::{self, Cursor, Page};
use crate::infra::db::world_tx;
use crate::infra::ids::new_id;
use crate::infra::timefmt::rfc3339;
use crate::state::AppState;

// Size caps per kind (§10.6). Thumb has its own small cap.
const PHOTO_MAX: i64 = 2 * 1024 * 1024;
const VIDEO_MAX: i64 = 25 * 1024 * 1024;
const AUDIO_MAX: i64 = 1024 * 1024;
const THUMB_MAX: i64 = 40 * 1024;

/// Live rows re-verified per world per tick (§10.6): the sweep is incremental,
/// never a full scan.
const VERIFY_BATCH: i64 = 500;
/// Concurrent HEADs during a verify batch (§10.6).
const VERIFY_CONCURRENCY: usize = 16;

/// `(max bytes, mime allowlist, issues a thumb target)` for a kind (§10.6).
fn caps(kind: MediaKind) -> (i64, &'static [&'static str], bool) {
    match kind {
        MediaKind::Photo => (PHOTO_MAX, &["image/jpeg", "image/png", "image/webp"], true),
        MediaKind::Video => (VIDEO_MAX, &["video/mp4", "video/webm"], true),
        MediaKind::Audio => (AUDIO_MAX, &["audio/mpeg", "audio/ogg", "audio/webm"], false),
    }
}

fn kind_str(k: MediaKind) -> &'static str {
    match k {
        MediaKind::Photo => "photo",
        MediaKind::Video => "video",
        MediaKind::Audio => "audio",
    }
}

fn parse_kind(s: &str) -> MediaKind {
    match s {
        "video" => MediaKind::Video,
        "audio" => MediaKind::Audio,
        // Unknown/legacy rows read as photo — the neutral default.
        _ => MediaKind::Photo,
    }
}

/// `media.request_upload` (§10.6): validate the kind/mime/size, record a
/// `pending` row, and return the POST policies. The row exists before the
/// object; `commit` promotes it once the client has uploaded.
pub async fn request_upload(
    state: &AppState,
    who: &Identity,
    kind: MediaKind,
    bytes: i64,
    mime: &str,
) -> Result<UploadTicket, Fail> {
    let (max, allow, has_thumb) = caps(kind);
    if bytes <= 0 || bytes > max {
        return Err(Fail::Code(ErrCode::TooLarge));
    }
    if !allow.contains(&mime) {
        return Err(Fail::Code(ErrCode::Invalid));
    }

    let media_id = new_id();
    let now = OffsetDateTime::now_utc();
    let mut tx = world_tx(&state.pg, who.world_id).await?;
    sqlx::query(
        "INSERT INTO media (id, world_id, owner_character, kind, mime, bytes, state, has_thumb) \
         VALUES ($1, $2, $3, $4, $5, $6, 'pending', $7)",
    )
    .bind(media_id)
    .bind(who.world_id)
    .bind(who.character_id)
    .bind(kind_str(kind))
    .bind(mime)
    .bind(bytes)
    .bind(has_thumb)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    let mut targets = Vec::with_capacity(2);
    let (url, fields) = state
        .s3
        .post_policy(
            &state.s3.object_key(who.world_id, media_id, false),
            mime,
            bytes,
            now,
        )
        .map_err(Fail::Internal)?;
    targets.push(UploadTarget {
        role: "original".into(),
        url,
        fields,
    });
    if has_thumb {
        // Thumbs are pinned to image/jpeg with a small cap: one fixed shape the
        // client renders galleries from, regardless of the original kind.
        // ponytail: jpeg-only thumb. Widen the thumb mime set only if a client
        // needs png/webp thumbs.
        let (turl, tfields) = state
            .s3
            .post_policy(
                &state.s3.object_key(who.world_id, media_id, true),
                "image/jpeg",
                THUMB_MAX,
                now,
            )
            .map_err(Fail::Internal)?;
        targets.push(UploadTarget {
            role: "thumb".into(),
            url: turl,
            fields: tfields,
        });
    }
    Ok(UploadTicket { media_id, targets })
}

/// `media.commit` (§10.6): promote the caller's own `pending` row to `live`.
/// `verified_at` is cleared so the verify sweep confirms the object exists
/// within a tick. A foreign row (same world, other owner) is `forbidden`; a
/// missing/other-world row is `not_found` (RLS hides it) — the two collapse to
/// what the caller is allowed to know.
pub async fn commit(state: &AppState, who: &Identity, media_id: Uuid) -> Result<(), Fail> {
    let mut tx = world_tx(&state.pg, who.world_id).await?;
    let updated: Option<i32> = sqlx::query_scalar(
        "UPDATE media SET state = 'live', verified_at = NULL \
         WHERE id = $1 AND owner_character = $2 AND state = 'pending' RETURNING 1",
    )
    .bind(media_id)
    .bind(who.character_id)
    .fetch_optional(&mut *tx)
    .await?;
    if updated.is_some() {
        tx.commit().await?;
        return Ok(());
    }
    // Distinguish "exists but not yours / not pending" (forbidden) from
    // "no such row in this world" (not_found) on the failure path only.
    let exists: Option<i32> = sqlx::query_scalar("SELECT 1 FROM media WHERE id = $1")
        .bind(media_id)
        .fetch_optional(&mut *tx)
        .await?;
    tx.commit().await?;
    if exists.is_some() {
        Err(Fail::Code(ErrCode::Forbidden))
    } else {
        Err(Fail::Code(ErrCode::NotFound))
    }
}

/// Attachment gate (roadmap Sprint 5 item 6): every id in a message's
/// `media_ids` must be a `live` row owned by the sender. Distinct owned-live
/// ids must equal distinct requested ids — a repeated or foreign id fails.
pub async fn all_owned_live(state: &AppState, who: &Identity, ids: &[Uuid]) -> Result<bool, Fail> {
    if ids.is_empty() {
        return Ok(true);
    }
    let mut distinct: Vec<Uuid> = ids.to_vec();
    distinct.sort();
    distinct.dedup();
    let mut tx = world_tx(&state.pg, who.world_id).await?;
    let cnt: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM media \
         WHERE id = ANY($1) AND owner_character = $2 AND state = 'live'",
    )
    .bind(&distinct)
    .bind(who.character_id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(cnt == distinct.len() as i64)
}

#[derive(sqlx::FromRow)]
struct MediaRow {
    id: Uuid,
    kind: String,
    mime: String,
    bytes: i64,
    has_thumb: bool,
    created_at: OffsetDateTime,
}

/// `GET /v1/media?cursor&limit` (§10.6): the caller's own live gallery,
/// newest-first on the shared cursor idiom (CDR-7). Each row carries fresh
/// presigned GET URLs — the client fetches bytes straight from S3.
pub async fn list(
    state: &AppState,
    who: &Identity,
    cursor: Option<Cursor>,
    limit: i64,
) -> Result<Page<MediaItem>, Fail> {
    let limit = limit.clamp(1, 100) as usize;
    let (cur_ts, cur_id) = match &cursor {
        Some(c) => (Some(c.ts), c.id),
        None => (None, Uuid::nil()),
    };
    let mut tx = world_tx(&state.pg, who.world_id).await?;
    let rows: Vec<MediaRow> = sqlx::query_as(
        "SELECT id, kind, mime, bytes, has_thumb, created_at FROM media \
         WHERE owner_character = $1 AND state = 'live' \
           AND ($2::timestamptz IS NULL OR (created_at, id) < ($2, $3)) \
         ORDER BY created_at DESC, id DESC LIMIT $4",
    )
    .bind(who.character_id)
    .bind(cur_ts)
    .bind(cur_id)
    .bind(limit as i64 + 1)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    let paged = cursor::page(rows, limit, |r| (r.created_at, r.id));
    let mut items = Vec::with_capacity(paged.items.len());
    for r in paged.items {
        let url = state
            .s3
            .presign_get(&state.s3.object_key(who.world_id, r.id, false))
            .map_err(Fail::Internal)?;
        let thumb_url = if r.has_thumb {
            Some(
                state
                    .s3
                    .presign_get(&state.s3.object_key(who.world_id, r.id, true))
                    .map_err(Fail::Internal)?,
            )
        } else {
            None
        };
        items.push(MediaItem {
            media_id: r.id,
            kind: parse_kind(&r.kind),
            mime: r.mime,
            bytes: r.bytes,
            url,
            thumb_url,
            created_at: rfc3339(r.created_at),
        });
    }
    Ok(Page {
        items,
        next_cursor: paged.next_cursor,
    })
}

/// Janitor (§10.6): pending rows older than 15 min → delete the row and
/// best-effort delete both objects. Idempotent and world-scoped under an
/// advisory lock (cross-cutting rule 7).
pub async fn reap_pending(state: &AppState, world: Uuid) -> anyhow::Result<u64> {
    let mut tx = world_tx(&state.pg, world).await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('janitor:media_pending_reap'))")
        .execute(&mut *tx)
        .await?;
    let rows: Vec<(Uuid, bool)> = sqlx::query_as(
        "DELETE FROM media WHERE state = 'pending' AND created_at < now() - interval '15 minutes' \
         RETURNING id, has_thumb",
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    for (id, has_thumb) in &rows {
        let _ = state
            .s3
            .delete(&state.s3.object_key(world, *id, false))
            .await;
        if *has_thumb {
            let _ = state
                .s3
                .delete(&state.s3.object_key(world, *id, true))
                .await;
        }
    }
    Ok(rows.len() as u64)
}

/// Janitor (§10.6): re-verify live rows not checked in 24 h (verified_at NULLS
/// FIRST). HEAD each object; a missing object or `content_length > declared
/// bytes` reverts the row to `pending` (the next reap deletes it) — this is the
/// mechanism that catches a client that bypassed the upload cap. Returns rows
/// reverted.
///
/// The advisory lock is released before the HEADs so the batch never holds a
/// pool connection across the network; two concurrent janitors would at worst
/// re-HEAD the same batch (wasteful, not wrong — the updates are idempotent).
pub async fn verify_live(state: &AppState, world: Uuid) -> anyhow::Result<u64> {
    let mut tx = world_tx(&state.pg, world).await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('janitor:media_verify'))")
        .execute(&mut *tx)
        .await?;
    let rows: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT id, bytes FROM media WHERE state = 'live' \
           AND (verified_at IS NULL OR verified_at < now() - interval '24 hours') \
         ORDER BY verified_at NULLS FIRST, id LIMIT $1",
    )
    .bind(VERIFY_BATCH)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    if rows.is_empty() {
        return Ok(0);
    }

    // Concurrent HEADs. Each yields Ok(true)=healthy, Ok(false)=revert,
    // Err=transient (leave for the next tick).
    let checks = stream::iter(rows.into_iter().map(|(id, declared)| async move {
        let key = state.s3.object_key(world, id, false);
        match state.s3.head(&key).await {
            Ok(Some(len)) => (id, Ok(len as i64 <= declared)),
            Ok(None) => (id, Ok(false)), // missing object
            Err(e) => (id, Err(e)),
        }
    }))
    .buffer_unordered(VERIFY_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    let mut ok_ids = Vec::new();
    let mut bad_ids = Vec::new();
    for (id, res) in checks {
        match res {
            Ok(true) => ok_ids.push(id),
            Ok(false) => bad_ids.push(id),
            Err(e) => tracing::warn!(error = %e, %id, "media verify HEAD failed"),
        }
    }

    let mut reverted = 0u64;
    let mut tx = world_tx(&state.pg, world).await?;
    if !ok_ids.is_empty() {
        sqlx::query("UPDATE media SET verified_at = now() WHERE id = ANY($1)")
            .bind(&ok_ids)
            .execute(&mut *tx)
            .await?;
    }
    if !bad_ids.is_empty() {
        reverted = sqlx::query(
            "UPDATE media SET state = 'pending', verified_at = NULL \
             WHERE id = ANY($1) AND state = 'live'",
        )
        .bind(&bad_ids)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    }
    tx.commit().await?;
    if reverted > 0 {
        counter!("opn_media_verify_reverted_total").increment(reverted);
    }
    Ok(reverted)
}
