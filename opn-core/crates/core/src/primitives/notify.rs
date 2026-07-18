//! notify primitive (OPN-CORE.md §10.8): the one routing choke point every
//! other primitive calls to reach a recipient. `route` decides live-push vs
//! durable inbox; callers only choose the semantic `class` (and pass a
//! `muted` flag when they know the recipient's membership).
//!
//! Presentation is entirely the shell's — Core stores/forwards the class and
//! nothing about how it looks (§10.8).

use contracts::types::InboxItem;
use contracts::{Evt, NotifyClass};
use metrics::counter;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use super::Fail;
use crate::infra::auth::Identity;
use crate::infra::cursor::{self, Cursor, Page};
use crate::infra::db::world_tx;
use crate::infra::ids::new_id;
use crate::infra::timefmt::rfc3339;
use crate::state::AppState;

/// A notification to deliver. `class` is the emitting primitive's call
/// (calls → ring, messages → alert, receipts/likes → silent); `route` may
/// downgrade it to `silent` for a muted channel.
pub struct Notification {
    pub app_id: String,
    pub kind: String,
    pub class: NotifyClass,
    pub payload: serde_json::Value,
}

/// Route one notification to `recipient` (a character) in `world`.
///
/// - Recipient has a live session → push `notify.event` on each of their
///   `notify:<device_id>` topics (§10.8). Nothing is stored.
/// - No live session → insert one `inbox` row, read via HTTP on next login.
///
/// `muted` (the recipient's `channel_members.muted`, when the caller has it)
/// strips alert urgency to `silent` — data still flows, the thread still
/// accumulates unread (§10.8 suppression split).
pub async fn route(
    state: &AppState,
    world: Uuid,
    recipient: Uuid,
    mut n: Notification,
    muted: bool,
) -> Result<(), Fail> {
    if muted {
        n.class = NotifyClass::Silent;
    }

    if state.registry.is_character_online(world, recipient) {
        let evt = Evt::NotifyEvent {
            app_id: n.app_id,
            kind: n.kind,
            class: n.class,
            payload: n.payload,
        };
        // ponytail: a recipient who races offline between this check and the
        // scan gets nothing (no push targets, not inboxed). Best-effort live
        // delivery by design — the durable truth lives in the channel/inbox.
        for device in state.registry.online_notify_targets(world, recipient) {
            crate::gateway::publish(state, world, &format!("notify:{device}"), &evt).await;
        }
    } else {
        let mut tx = world_tx(&state.pg, world).await?;
        sqlx::query(
            "INSERT INTO inbox (id, world_id, character_id, app_id, kind, class, payload) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(new_id())
        .bind(world)
        .bind(recipient)
        .bind(&n.app_id)
        .bind(&n.kind)
        .bind(class_str(n.class))
        .bind(&n.payload)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        counter!("opn_inbox_inserts_total").increment(1);
    }
    Ok(())
}

/// `notify.seen { ids }` — mark the caller's own inbox rows seen (idempotent;
/// already-seen and foreign ids are silently skipped by the predicate).
pub async fn seen(pool: &PgPool, who: &Identity, ids: &[Uuid]) -> Result<(), Fail> {
    if ids.is_empty() {
        return Ok(());
    }
    let mut tx = world_tx(pool, who.world_id).await?;
    sqlx::query(
        "UPDATE inbox SET seen_at = now() \
         WHERE character_id = $1 AND id = ANY($2) AND seen_at IS NULL",
    )
    .bind(who.character_id)
    .bind(ids)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// `notify.clear` — drop all of the caller's inbox rows.
pub async fn clear(pool: &PgPool, who: &Identity) -> Result<(), Fail> {
    let mut tx = world_tx(pool, who.world_id).await?;
    sqlx::query("DELETE FROM inbox WHERE character_id = $1")
        .bind(who.character_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct InboxRow {
    id: Uuid,
    app_id: String,
    kind: String,
    class: String,
    payload: serde_json::Value,
    seen_at: Option<OffsetDateTime>,
    created_at: OffsetDateTime,
}

/// `GET /v1/notify/inbox?cursor&limit` — newest-first inbox page on the shared
/// cursor idiom (CDR-7, roadmap Sprint 4 item 1, closing Sprint 3's TODO).
/// Keysets on `(created_at, id)` descending; overfetches one row so `page`
/// can emit the next cursor.
pub async fn inbox_list(
    pool: &PgPool,
    who: &Identity,
    cursor: Option<Cursor>,
    limit: i64,
) -> Result<Page<InboxItem>, Fail> {
    let limit = limit.clamp(1, 100) as usize;
    // NULL cursor ts short-circuits the predicate; the id bind is then unused.
    let (cur_ts, cur_id) = match &cursor {
        Some(c) => (Some(c.ts), c.id),
        None => (None, Uuid::nil()),
    };
    let mut tx = world_tx(pool, who.world_id).await?;
    let rows: Vec<InboxRow> = sqlx::query_as(
        "SELECT id, app_id, kind, class, payload, seen_at, created_at FROM inbox \
         WHERE character_id = $1 \
           AND ($2::timestamptz IS NULL OR (created_at, id) < ($2, $3)) \
         ORDER BY created_at DESC, id DESC LIMIT $4",
    )
    .bind(who.character_id)
    .bind(cur_ts)
    .bind(cur_id)
    .bind(limit as i64 + 1)
    .fetch_all(&mut *tx)
    .await?;

    // Page over the raw rows (they carry the OffsetDateTime the cursor needs),
    // then project to the wire type.
    let paged = cursor::page(rows, limit, |r| (r.created_at, r.id));
    Ok(Page {
        items: paged
            .items
            .into_iter()
            .map(|r| InboxItem {
                id: r.id,
                app_id: r.app_id,
                kind: r.kind,
                class: parse_class(&r.class),
                payload: r.payload,
                seen_at: r.seen_at.map(rfc3339),
                created_at: rfc3339(r.created_at),
            })
            .collect(),
        next_cursor: paged.next_cursor,
    })
}

fn class_str(c: NotifyClass) -> &'static str {
    match c {
        NotifyClass::Ring => "ring",
        NotifyClass::Alert => "alert",
        NotifyClass::Silent => "silent",
    }
}

fn parse_class(s: &str) -> NotifyClass {
    match s {
        "ring" => NotifyClass::Ring,
        "silent" => NotifyClass::Silent,
        // Unknown/legacy rows read as alert — the neutral default.
        _ => NotifyClass::Alert,
    }
}
