//! Presence (§4.2, CDR-6): Redis key is the cross-replica/introspection
//! truth, registry counts drive transitions, `share_presence` gates every
//! read and emit. Single replica: transitions are local registry events.

use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use contracts::Evt;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

use super::registry::ConnHandle;
use crate::infra::db::world_tx;
use crate::primitives::Fail;
use crate::state::AppState;

fn key(world: Uuid, character: Uuid) -> String {
    format!("presence:{world}:{character}")
}

/// Connect transition: first live connection of a character marks it online
/// (Redis + event). Characters with `share_presence` off never touch either.
pub async fn on_connect(state: &AppState, handle: &std::sync::Arc<ConnHandle>, came_online: bool) {
    if !came_online || !handle.share_presence.load(Ordering::Relaxed) {
        return;
    }
    let id = &handle.identity;
    let mut redis = state.redis.clone();
    let set: Result<(), redis::RedisError> = redis::cmd("SET")
        .arg(key(id.world_id, id.character_id))
        .arg(1)
        .arg("EX")
        .arg(90)
        .query_async(&mut redis)
        .await;
    if let Err(e) = set {
        tracing::warn!(error = %e, "presence SET failed");
    }
    let evt = Evt::PresenceState {
        character_id: id.character_id,
        online: Some(true),
        last_seen_at: None,
    };
    super::publish(
        state,
        id.world_id,
        &format!("presence:{}", id.character_id),
        &evt,
    )
    .await;
}

/// Disconnect: `last_seen_at` is always recorded; the offline transition
/// (Redis DEL + event) fires only when the *last* connection went away.
pub async fn on_disconnect(
    state: &AppState,
    handle: &std::sync::Arc<ConnHandle>,
    went_offline: bool,
) {
    let id = &handle.identity;
    let last_seen = OffsetDateTime::now_utc();
    let update = async {
        let mut tx = world_tx(&state.pg, id.world_id).await?;
        sqlx::query("UPDATE characters SET last_seen_at = now() WHERE id = $1")
            .bind(id.character_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        anyhow::Ok(())
    };
    if let Err(e) = update.await {
        tracing::warn!(error = %e, "last_seen_at update failed");
    }
    if !went_offline {
        return;
    }
    let mut redis = state.redis.clone();
    let del: Result<(), redis::RedisError> = redis::cmd("DEL")
        .arg(key(id.world_id, id.character_id))
        .query_async(&mut redis)
        .await;
    if let Err(e) = del {
        tracing::warn!(error = %e, "presence DEL failed");
    }
    if handle.share_presence.load(Ordering::Relaxed) {
        let evt = Evt::PresenceState {
            character_id: id.character_id,
            online: Some(false),
            last_seen_at: last_seen.format(&Rfc3339).ok(),
        };
        super::publish(
            state,
            id.world_id,
            &format!("presence:{}", id.character_id),
            &evt,
        )
        .await;
    }
}

/// Snapshot-on-sub (§4.2): pushed to the subscriber before the sub ack.
/// `share_presence` off → `online: null` — same bytes whether they are
/// connected or not.
pub async fn snapshot(state: &AppState, world: Uuid, character: Uuid) -> Result<Evt, Fail> {
    let mut tx = world_tx(&state.pg, world).await?;
    let row: Option<(bool, Option<OffsetDateTime>)> =
        sqlx::query_as("SELECT share_presence, last_seen_at FROM characters WHERE id = $1")
            .bind(character)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((share, last_seen_at)) = row else {
        return Err(Fail::Code(contracts::ErrCode::NotFound));
    };
    if !share {
        return Ok(Evt::PresenceState {
            character_id: character,
            online: None,
            last_seen_at: None,
        });
    }
    let online = state.registry.is_character_online(world, character);
    Ok(Evt::PresenceState {
        character_id: character,
        online: Some(online),
        last_seen_at: if online {
            None
        } else {
            last_seen_at.and_then(|t| t.format(&Rfc3339).ok())
        },
    })
}

/// Keeps `presence:*` keys alive: one pipelined pass over local sessions per
/// heartbeat tick (§4.2 — never per-connection round trips).
pub fn spawn_refresher(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(state.cfg.heartbeat_secs));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let live = state.registry.live_characters();
            if live.is_empty() {
                continue;
            }
            let mut pipe = redis::pipe();
            for (world, character, share) in live {
                if share {
                    pipe.cmd("SET")
                        .arg(key(world, character))
                        .arg(1)
                        .arg("EX")
                        .arg(90)
                        .ignore();
                }
            }
            let mut redis = state.redis.clone();
            if let Err(e) = pipe.query_async::<()>(&mut redis).await {
                tracing::warn!(error = %e, "presence refresh pipeline failed");
            }
        }
    })
}
