use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

use crate::types::MessageBody;

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
