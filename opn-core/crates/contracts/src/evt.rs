use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

use crate::types::NotifyClass;

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

    /// A new message, fanned out on `ch:<channel_id>` (§8). Durable: a slow
    /// consumer that cannot take it is closed, then re-syncs via resume/history.
    #[serde(rename = "channels.message")]
    ChannelsMessage {
        channel_id: Uuid,
        message_id: Uuid,
        #[ts(type = "number")]
        seq: i64,
        sender: Uuid,
        #[ts(type = "unknown")]
        body: serde_json::Value,
        /// RFC 3339 server timestamp.
        at: String,
    },

    /// A notification pushed to a live recipient on `notify:<device_id>`
    /// (§10.8). Offline recipients get an `inbox` row instead — see
    /// `notify::route`.
    #[serde(rename = "notify.event")]
    NotifyEvent {
        app_id: String,
        kind: String,
        class: NotifyClass,
        #[ts(type = "unknown")]
        payload: serde_json::Value,
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
            // Messages are the durable spine; a dropped one is a lost message
            // until resume, so close the slow consumer instead (§4.3, §8).
            Evt::ChannelsMessage { .. } => EvtClass::Durable,
            // Notifications are the product's core signal (a silently dropped
            // ring/alert is exactly the degradation ADR-1 forbids). Durable:
            // close a consumer too slow for them; it re-syncs the durable
            // truth on reconnect (channel watermarks, inbox, /calls/active).
            Evt::NotifyEvent { .. } => EvtClass::Durable,
        }
    }
}
