use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

use crate::types::{CallKind, CallParticipant, CallSessionState, NotifyClass, ReceiptKind};

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

    /// A member's delivered/read watermark moved (§10.2), fanned out on
    /// `ch:<channel_id>`. Durable: it drives unread counts, which must not
    /// silently desync.
    #[serde(rename = "channels.receipt")]
    ChannelsReceipt {
        channel_id: Uuid,
        character_id: Uuid,
        kind: ReceiptKind,
        #[ts(type = "number")]
        up_to_seq: i64,
        /// RFC 3339 server timestamp.
        at: String,
    },

    /// "Is typing" ping on `ch:<channel_id>` (§10.2). Ephemeral: a lost one
    /// costs nothing, and a slow consumer must not be closed over it.
    #[serde(rename = "channels.typing")]
    ChannelsTyping {
        channel_id: Uuid,
        character_id: Uuid,
    },

    /// A reaction was added or removed (§10.2). Durable — reaction state is
    /// part of the message the client renders.
    #[serde(rename = "channels.reaction")]
    ChannelsReaction {
        channel_id: Uuid,
        message_id: Uuid,
        character_id: Uuid,
        emoji: String,
        added: bool,
    },

    /// A message was pinned or unpinned (§10.2). Durable.
    #[serde(rename = "channels.pin")]
    ChannelsPin {
        channel_id: Uuid,
        message_id: Uuid,
        by: Uuid,
        pinned: bool,
    },

    /// A group's membership changed (§10.2). Durable.
    #[serde(rename = "channels.member")]
    ChannelsMember {
        channel_id: Uuid,
        character_id: Uuid,
        added: bool,
    },

    /// Resume replay hit its 500-row cap (§4.4): the client's gap is larger
    /// than one replay, so it should cold-load history over HTTP. Durable.
    #[serde(rename = "channels.resume_overflow")]
    ChannelsResumeOverflow { channel_id: Uuid },

    /// Full call-session snapshot on `call:<id>` (§10.4): pushed on every state
    /// change and once on subscribe (snapshot-on-sub, CDR-6). Full-state by
    /// design — small, and it kills the delta-desync class of bug. Durable: a
    /// dropped snapshot leaves the client's call UI wrong until re-sync.
    #[serde(rename = "calls.state")]
    CallsState {
        call_id: Uuid,
        kind: CallKind,
        state: CallSessionState,
        participants: Vec<CallParticipant>,
    },

    /// Opaque WebRTC signaling relay on `call:<id>` (§10.4): offer/answer/ICE
    /// forwarded between participants, never inspected. Durable — a dropped ICE
    /// candidate stalls call setup, so the queue-full close is the correct
    /// failure (a consumer too slow for signaling was not completing the call).
    #[serde(rename = "calls.signal")]
    CallsSignal {
        call_id: Uuid,
        from: Uuid,
        to: Uuid,
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
            // Receipts drive unread counts; a dropped one desyncs the badge
            // until a full re-list. Close the slow consumer instead.
            Evt::ChannelsReceipt { .. } => EvtClass::Durable,
            // Typing is pure presentation garnish — drop it under pressure.
            Evt::ChannelsTyping { .. } => EvtClass::Ephemeral,
            // Reaction/pin/member changes are part of the durable channel
            // state the client renders; a lost one is a wrong render until
            // re-sync, so close rather than drop.
            Evt::ChannelsReaction { .. } => EvtClass::Durable,
            Evt::ChannelsPin { .. } => EvtClass::Durable,
            Evt::ChannelsMember { .. } => EvtClass::Durable,
            // The "cold-load, you overflowed" signal must arrive or the client
            // silently keeps a gap.
            Evt::ChannelsResumeOverflow { .. } => EvtClass::Durable,
            // Full call snapshots drive the call UI; a dropped one desyncs it
            // until re-sync, so close a consumer too slow for it (§10.4).
            Evt::CallsState { .. } => EvtClass::Durable,
            // A dropped ICE candidate stalls setup — close rather than drop
            // (§10.4: signaling is durable, unlike the ephemeral garnish above).
            Evt::CallsSignal { .. } => EvtClass::Durable,
        }
    }
}
