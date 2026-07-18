use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

/// Every server→client pushed event. Same tagging idiom as `Cmd`.
///
/// Variants land with their primitives (first ones in Sprint 2/3); the enum
/// exists from day one so the contracts drift gate and the coverage
/// match-test exist from day one.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "evt", content = "payload", rename_all = "snake_case")]
#[ts(export)]
pub enum Evt {
    /// Presence transition or snapshot-on-sub (§4.2, CDR-6). `online: null`
    /// means the character does not share presence — indistinguishable from
    /// the wire's point of view whether they are connected.
    #[serde(rename = "presence.state")]
    PresenceState {
        character_id: Uuid,
        online: Option<bool>,
        /// RFC 3339; present only in `online: false` transitions.
        last_seen_at: Option<String>,
    },
}

/// Backpressure class (OPN-CORE.md §4.3): durable events close a slow
/// consumer when its queue is full; ephemeral events are dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvtClass {
    Durable,
    Ephemeral,
}

impl Evt {
    /// Exhaustive by construction: every new event variant fails to compile
    /// until it declares its backpressure class here.
    pub fn class(&self) -> EvtClass {
        match self {
            // A lost presence flip costs a re-sub snapshot, nothing more.
            Evt::PresenceState { .. } => EvtClass::Ephemeral,
        }
    }
}
