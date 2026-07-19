//! WS gateway (OPN-CORE.md §4): connection lifecycle, registry, dispatch,
//! presence, cross-replica fan-out.

use contracts::Evt;
use uuid::Uuid;

use crate::state::AppState;

pub mod dispatch;
pub mod fanout;
pub mod link;
pub mod presence;
pub mod registry;
pub mod topic;
pub mod ws;

/// The one publish entry point for primitives: local fan-out always, Redis
/// `PUBLISH` only when running with replicas (§3, §8). `fanout::listen` on
/// the other replicas turns that back into `publish_local`.
pub async fn publish(state: &AppState, world: Uuid, topic: &str, evt: &Evt) {
    state.registry.publish_local(world, topic, evt);
    if state.cfg.replicas > 1 {
        fanout::publish_remote(state, world, topic, evt).await;
    }
}
