use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

use crate::types::{MediaKind, MessageBody};

/// Every client→server command. Serde does the routing, `match` does the
/// dispatch, the compiler does the exhaustiveness (OPN-CORE.md §7).
///
/// Wire shape: `{ "cmd": "<name>", "payload": { ... } }` (unit variants omit
/// `payload`). `sub`/`unsub` come from the enum-level `rename_all`; the
/// dotted `auth.refresh`/`identity.*` names are pinned per-variant to match
/// the design doc (OPN-CORE.md §10.1).
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "cmd", content = "payload", rename_all = "snake_case")]
#[ts(export)]
pub enum Cmd {
    /// First frame on a fresh WS connection (§4.1): carries the mint/refresh
    /// JWT. Sent again after auth it acks `conflict`.
    Auth {
        token: String,
    },
    Sub {
        topic: String,
        #[ts(type = "number | null")]
        last_seq: Option<i64>,
    },
    Unsub {
        topic: String,
    },
    #[serde(rename = "auth.refresh")]
    AuthRefresh,
    #[serde(rename = "identity.me")]
    IdentityMe,
    #[serde(rename = "identity.app_login")]
    IdentityAppLogin {
        app_id: String,
        account_id: Uuid,
    },
    #[serde(rename = "identity.get_settings")]
    IdentityGetSettings {
        scope: SettingsScope,
    },
    #[serde(rename = "identity.set_settings")]
    IdentitySetSettings {
        scope: SettingsScope,
        /// Whole-document replace, opaque to Core.
        #[ts(type = "unknown")]
        patch: serde_json::Value,
    },
    #[serde(rename = "identity.set_share_presence")]
    IdentitySetSharePresence {
        on: bool,
    },

    // ── channels (§10.2) ─────────────────────────────────────────────────
    /// Persist + sequence + fan out a message (§8). `client_uuid` is the
    /// caller-chosen idempotency key: a retry with the same one returns the
    /// original ack and fans out nothing.
    #[serde(rename = "channels.send")]
    ChannelsSend {
        channel_id: Uuid,
        client_uuid: Uuid,
        body: MessageBody,
    },
    /// Found-or-create the pair thread to a phone number (§10.2). Resolves the
    /// number through the directory seam; a blocked/unknown number is
    /// `not_found` either way (privacy, §10.7).
    #[serde(rename = "channels.open_direct")]
    ChannelsOpenDirect {
        number: String,
    },
    /// Create a group: creator + explicit member list (≤ 32), kind=group.
    #[serde(rename = "channels.create")]
    ChannelsCreate {
        #[ts(type = "string | null")]
        name: Option<String>,
        members: Vec<Uuid>,
    },
    /// Snapshot of the caller's memberships (channel + own watermarks +
    /// last-message preview).
    #[serde(rename = "channels.list")]
    ChannelsList,
    /// Advance the caller's delivered watermark (§10.2). Monotonic + idempotent;
    /// emits `channels.receipt` only when it actually moves.
    #[serde(rename = "channels.mark_delivered")]
    ChannelsMarkDelivered {
        channel_id: Uuid,
        #[ts(type = "number")]
        up_to_seq: i64,
    },
    /// Advance the caller's read watermark (§10.2). Same monotonic rule.
    #[serde(rename = "channels.mark_read")]
    ChannelsMarkRead {
        channel_id: Uuid,
        #[ts(type = "number")]
        up_to_seq: i64,
    },
    /// Ephemeral "is typing" ping (§10.2): fanned out, never stored. The client
    /// self-limits to ~1/3 s; the rate bucket handles abuse.
    #[serde(rename = "channels.typing")]
    ChannelsTyping {
        channel_id: Uuid,
    },
    /// Add a reaction, keyed `(message_id, character, emoji)` (§10.2).
    #[serde(rename = "channels.react")]
    ChannelsReact {
        channel_id: Uuid,
        message_id: Uuid,
        emoji: String,
    },
    /// Remove one of the caller's reactions.
    #[serde(rename = "channels.unreact")]
    ChannelsUnreact {
        channel_id: Uuid,
        message_id: Uuid,
        emoji: String,
    },
    /// Pin a message (§10.2), cap 50 per channel (`conflict` at the cap).
    #[serde(rename = "channels.pin")]
    ChannelsPin {
        channel_id: Uuid,
        message_id: Uuid,
    },
    #[serde(rename = "channels.unpin")]
    ChannelsUnpin {
        channel_id: Uuid,
        message_id: Uuid,
    },
    /// Add a member to a group (§10.2, group kind only). Any member may add.
    #[serde(rename = "channels.member_add")]
    ChannelsMemberAdd {
        channel_id: Uuid,
        character_id: Uuid,
    },
    /// Remove a member from a group. Any member may remove; the removed
    /// member's live subscription is dropped server-side.
    #[serde(rename = "channels.member_remove")]
    ChannelsMemberRemove {
        channel_id: Uuid,
        character_id: Uuid,
    },

    // ── media (§10.6) ────────────────────────────────────────────────────
    /// Request a presigned upload (§10.6): validates the kind/mime pair and the
    /// size cap, inserts a `pending` row, and returns S3 POST policies (one for
    /// the original, one for the thumb on photo/video). The policy's
    /// `content-length-range` makes the cap MinIO-enforced, not advisory
    /// (OPN.md §7.2). `commit` promotes to `live`.
    #[serde(rename = "media.request_upload")]
    MediaRequestUpload {
        kind: MediaKind,
        #[ts(type = "number")]
        bytes: i64,
        mime: String,
    },
    /// Promote the caller's own `pending` upload to `live` (§10.6). Owner-checked;
    /// no synchronous HEAD — the janitor verifies the object out of band (§17.3).
    #[serde(rename = "media.commit")]
    MediaCommit {
        media_id: Uuid,
    },

    // ── notify (§10.8) ───────────────────────────────────────────────────
    #[serde(rename = "notify.seen")]
    NotifySeen {
        ids: Vec<Uuid>,
    },
    #[serde(rename = "notify.clear")]
    NotifyClear,
}

/// Which settings document an `identity.*_settings` command targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum SettingsScope {
    Device,
    Character,
}
