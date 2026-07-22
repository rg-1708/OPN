//! servers primitive (OPN-CORE.md §10.2a, contract gap #13): channel
//! containers for the guild-style UX. A server is a membership umbrella over
//! ordinary channels — server channels ARE channels, and their
//! `channel_members` rows mirror `server_members`, so every downstream path
//! (send, history, receipts, reactions, resume, topic authz, RLS) is
//! untouched. This module owns only the container CRUD and the mirror sync.

use contracts::types::ServerSummary;
use contracts::{ErrCode, NotifyClass};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use super::notify::{self, Notification};
use super::Fail;
use crate::infra::auth::Identity;
use crate::infra::db::world_tx;
use crate::infra::ids::new_id;
use crate::infra::timefmt::rfc3339;
use crate::state::AppState;

/// Caps (§10.2a). Server membership is deliberately larger than the plain
/// group cap: a guild is the one place a big roster is the point.
const NAME_MAX: usize = 128;
const CATEGORY_MAX: usize = 64;
const SERVER_MEMBERS_MAX: i64 = 200;
const SERVER_CHANNELS_MAX: i64 = 50;

/// `servers.create`: insert the server + the owner's membership.
pub async fn create(
    state: &AppState,
    who: &Identity,
    name: &str,
    banner_media_id: Option<Uuid>,
) -> Result<serde_json::Value, Fail> {
    if name.is_empty() || name.len() > NAME_MAX {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    let mut tx = world_tx(&state.pg, who.world_id).await?;

    if let Some(banner) = banner_media_id {
        let live: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM media WHERE id = $1 AND state = 'live'")
                .bind(banner)
                .fetch_optional(&mut *tx)
                .await?;
        if live.is_none() {
            return Err(Fail::Code(ErrCode::Invalid));
        }
    }

    let server_id = new_id();
    sqlx::query(
        "INSERT INTO servers (id, world_id, name, banner_media_id, owner_character) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(server_id)
    .bind(who.world_id)
    .bind(name)
    .bind(banner_media_id)
    .bind(who.character_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO server_members (server_id, world_id, character_id) VALUES ($1, $2, $3)",
    )
    .bind(server_id)
    .bind(who.world_id)
    .bind(who.character_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(json!({ "server_id": server_id }))
}

#[derive(sqlx::FromRow)]
struct ServerRow {
    id: Uuid,
    name: String,
    banner_media_id: Option<Uuid>,
    owner_character: Uuid,
    member_count: i64,
    joined_at: OffsetDateTime,
}

/// `servers.list`: the caller's server memberships, oldest joined first.
pub async fn list(state: &AppState, who: &Identity) -> Result<Vec<ServerSummary>, Fail> {
    let mut tx = world_tx(&state.pg, who.world_id).await?;
    let rows: Vec<ServerRow> = sqlx::query_as(
        "SELECT s.id, s.name, s.banner_media_id, s.owner_character, m.joined_at, \
                (SELECT count(*) FROM server_members c WHERE c.server_id = s.id) AS member_count \
         FROM server_members m JOIN servers s ON s.id = m.server_id \
         WHERE m.character_id = $1 ORDER BY m.joined_at, s.id",
    )
    .bind(who.character_id)
    .fetch_all(&mut *tx)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| ServerSummary {
            server_id: r.id,
            name: r.name,
            banner_media_id: r.banner_media_id,
            owner_character_id: r.owner_character,
            member_count: r.member_count,
            joined_at: rfc3339(r.joined_at),
        })
        .collect())
}

/// `servers.member_add` / `servers.member_remove`: change one membership and
/// mirror it into every channel of the server. Add is owner-only; remove is
/// owner-removes-anyone-else or self-leave; the owner cannot leave. The
/// affected character learns of it via `notify.event` (app_id `servers`).
pub async fn member_change(
    state: &AppState,
    who: &Identity,
    server_id: Uuid,
    target: Uuid,
    add: bool,
) -> Result<(), Fail> {
    let mut tx = world_tx(&state.pg, who.world_id).await?;

    // Lock the server so concurrent member/channel sync can't interleave
    // (channel_create takes the same lock before mirroring the roster).
    let row: Option<(String, Uuid)> =
        sqlx::query_as("SELECT name, owner_character FROM servers WHERE id = $1 FOR UPDATE")
            .bind(server_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((name, owner)) = row else {
        return Err(Fail::Code(ErrCode::NotFound));
    };

    let changed = if add {
        if who.character_id != owner {
            return Err(Fail::Code(ErrCode::Forbidden));
        }
        let exists: Option<i32> = sqlx::query_scalar("SELECT 1 FROM characters WHERE id = $1")
            .bind(target)
            .fetch_optional(&mut *tx)
            .await?;
        if exists.is_none() {
            return Err(Fail::Code(ErrCode::Invalid));
        }
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM server_members WHERE server_id = $1")
            .bind(server_id)
            .fetch_one(&mut *tx)
            .await?;
        if count >= SERVER_MEMBERS_MAX {
            return Err(Fail::Code(ErrCode::Conflict));
        }
        let inserted = sqlx::query(
            "INSERT INTO server_members (server_id, world_id, character_id) \
             VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(server_id)
        .bind(who.world_id)
        .bind(target)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        if inserted {
            // Mirror into every channel of the server. Watermarks start fresh.
            sqlx::query(
                "INSERT INTO channel_members (channel_id, world_id, character_id) \
                 SELECT c.id, $2, $3 FROM channels c WHERE c.server_id = $1 \
                 ON CONFLICT DO NOTHING",
            )
            .bind(server_id)
            .bind(who.world_id)
            .bind(target)
            .execute(&mut *tx)
            .await?;
        }
        inserted
    } else {
        if target == owner {
            // The owner leaving would orphan the server; transfer is out of scope.
            return Err(Fail::Code(ErrCode::Conflict));
        }
        if who.character_id != owner && who.character_id != target {
            return Err(Fail::Code(ErrCode::Forbidden));
        }
        let removed = sqlx::query(
            "DELETE FROM server_members WHERE server_id = $1 AND character_id = $2",
        )
        .bind(server_id)
        .bind(target)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        if removed {
            sqlx::query(
                "DELETE FROM channel_members WHERE character_id = $2 \
                 AND channel_id IN (SELECT id FROM channels WHERE server_id = $1)",
            )
            .bind(server_id)
            .bind(target)
            .execute(&mut *tx)
            .await?;
        }
        removed
    };

    // Channel ids for the sub-drop, read before commit (same snapshot).
    let channel_ids: Vec<Uuid> = if changed && !add {
        sqlx::query_scalar("SELECT id FROM channels WHERE server_id = $1")
            .bind(server_id)
            .fetch_all(&mut *tx)
            .await?
    } else {
        Vec::new()
    };
    tx.commit().await?;

    if changed {
        // A removed member's sockets stop getting the server's channel traffic.
        for channel_id in &channel_ids {
            state.registry.drop_character_topic(
                who.world_id,
                target,
                &format!("ch:{channel_id}"),
            );
        }
        // Silent by class — the rail refreshes, nothing buzzes. Self-leave
        // needs no note-to-self.
        if target != who.character_id {
            notify::route(
                state,
                who.world_id,
                target,
                Notification {
                    app_id: "servers".into(),
                    kind: if add {
                        "server_member_added".into()
                    } else {
                        "server_member_removed".into()
                    },
                    class: NotifyClass::Silent,
                    payload: json!({ "server_id": server_id, "server_name": name }),
                },
                false,
            )
            .await?;
        }
    }
    Ok(())
}

/// `servers.channel_create` (owner only): a channel owned by the server, with
/// the current roster mirrored in. `voice` is a marker kind — audio rides the
/// existing group-call primitive; the channel still carries messages.
pub async fn channel_create(
    state: &AppState,
    who: &Identity,
    server_id: Uuid,
    name: &str,
    kind: &str,
    category: Option<&str>,
    position: i32,
) -> Result<serde_json::Value, Fail> {
    if name.is_empty() || name.len() > NAME_MAX {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    if !matches!(kind, "group" | "voice") {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    if category.is_some_and(|c| c.is_empty() || c.len() > CATEGORY_MAX) {
        return Err(Fail::Code(ErrCode::Invalid));
    }

    let mut tx = world_tx(&state.pg, who.world_id).await?;
    let owner: Option<Uuid> =
        sqlx::query_scalar("SELECT owner_character FROM servers WHERE id = $1 FOR UPDATE")
            .bind(server_id)
            .fetch_optional(&mut *tx)
            .await?;
    match owner {
        None => return Err(Fail::Code(ErrCode::NotFound)),
        Some(o) if o != who.character_id => return Err(Fail::Code(ErrCode::Forbidden)),
        Some(_) => {}
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM channels WHERE server_id = $1")
        .bind(server_id)
        .fetch_one(&mut *tx)
        .await?;
    if count >= SERVER_CHANNELS_MAX {
        return Err(Fail::Code(ErrCode::Conflict));
    }

    let channel_id = new_id();
    sqlx::query(
        "INSERT INTO channels (id, world_id, kind, name, server_id, category, position) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(channel_id)
    .bind(who.world_id)
    .bind(kind)
    .bind(name)
    .bind(server_id)
    .bind(category)
    .bind(position)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO channel_members (channel_id, world_id, character_id) \
         SELECT $1, $2, m.character_id FROM server_members m WHERE m.server_id = $3",
    )
    .bind(channel_id)
    .bind(who.world_id)
    .bind(server_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(json!({ "channel_id": channel_id }))
}
