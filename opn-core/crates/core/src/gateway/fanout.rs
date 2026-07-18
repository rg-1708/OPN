//! Cross-replica event fan-out over Redis pub/sub (OPN-CORE.md §3, §8).
//!
//! `gateway::publish` already did the local fan-out before it calls here, so
//! every replica must DROP its own re-delivered messages — otherwise every
//! event double-delivers on the origin replica. The `from` field carries a
//! process-level replica id for exactly that self-drop check.
//!
//! Best-effort by design: a failed `PUBLISH` or a dropped subscriber is logged
//! and swallowed. Durable-queue guarantees are per-connection (the registry's
//! send queue), not cross-replica — replicas come and go, and the presence
//! model tolerates a lost flip (§4.3).

use std::sync::OnceLock;

use contracts::Evt;
use futures_util::StreamExt;
use serde::Deserialize;
use uuid::Uuid;

use crate::state::AppState;

/// Channel prefix; the world uuid and topic follow: `opn:<world>:<topic>`.
const CHANNEL_PREFIX: &str = "opn:";

/// Replica id stamped into every published message so the origin replica can
/// drop its own echo.
///
/// A per-process random UUID gives cross-process uniqueness; it is XOR-mixed
/// with the `AppState`'s registry `Arc` pointer so two *in-process* replicas
/// (the roadmap's own two-instances-one-Redis test topology, and nothing in
/// production) still get distinct ids. A cloned `AppState` shares the registry
/// `Arc`, so `publish_remote` and this state's `spawn_listener` agree.
fn replica_id(state: &AppState) -> Uuid {
    static BASE: OnceLock<Uuid> = OnceLock::new();
    let base = BASE.get_or_init(crate::infra::ids::new_id).as_u128();
    let ptr = std::sync::Arc::as_ptr(&state.registry) as usize as u128;
    Uuid::from_u128(base ^ ptr)
}

/// Wire payload: `{ "from": "<replica_uuid>", "evt": <serialized Evt> }`.
#[derive(Deserialize)]
struct Wire {
    from: Uuid,
    evt: Evt,
}

pub async fn publish_remote(state: &AppState, world: Uuid, topic: &str, evt: &Evt) {
    let channel = format!("{CHANNEL_PREFIX}{world}:{topic}");
    let payload = serde_json::json!({ "from": replica_id(state).to_string(), "evt": evt });
    // Serialize once; contracts types serialize infallibly, but stay total.
    let payload = match serde_json::to_string(&payload) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "fanout: payload serialization failed");
            return;
        }
    };
    let mut conn = state.redis.clone();
    let published: Result<(), redis::RedisError> = redis::cmd("PUBLISH")
        .arg(&channel)
        .arg(&payload)
        .query_async(&mut conn)
        .await;
    if let Err(e) = published {
        tracing::warn!(error = %e, channel = %channel, "fanout: PUBLISH failed (best-effort)");
    }
}

pub fn spawn_listener(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Reconnect forever: a dropped pub/sub connection must never kill the
        // listener, or this replica silently stops receiving remote events.
        loop {
            if let Err(e) = run_listener(&state).await {
                tracing::warn!(error = %e, "fanout: listener connection ended, reconnecting");
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    })
}

/// One connection's lifetime. Returns `Err` on any setup/stream failure so the
/// caller sleeps and reconnects; returns `Ok` only if the stream ends cleanly.
async fn run_listener(state: &AppState) -> Result<(), redis::RedisError> {
    // The shared ConnectionManager cannot psubscribe — pub/sub needs a
    // dedicated connection.
    let client = redis::Client::open(state.cfg.redis_url.as_str())?;
    let mut pubsub = client.get_async_pubsub().await?;
    pubsub.psubscribe(format!("{CHANNEL_PREFIX}*")).await?;

    let me = replica_id(state);
    let mut stream = pubsub.on_message();
    while let Some(msg) = stream.next().await {
        let channel = msg.get_channel_name();
        let payload: String = match msg.get_payload() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, channel = %channel, "fanout: unreadable payload");
                continue;
            }
        };
        // Parse `opn:<world>:<topic>` — topic may itself contain ':'.
        let Some((world_str, topic)) = channel
            .strip_prefix(CHANNEL_PREFIX)
            .and_then(|rest| rest.split_once(':'))
        else {
            tracing::warn!(channel = %channel, "fanout: malformed channel");
            continue;
        };
        let Ok(world) = Uuid::parse_str(world_str) else {
            tracing::warn!(channel = %channel, "fanout: bad world uuid in channel");
            continue;
        };
        let wire: Wire = match serde_json::from_str(&payload) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(error = %e, channel = %channel, "fanout: bad payload json");
                continue;
            }
        };
        // Self-drop: the origin replica already fanned this out locally.
        if wire.from == me {
            continue;
        }
        state.registry.publish_local(world, topic, &wire.evt);
    }
    Ok(())
}
