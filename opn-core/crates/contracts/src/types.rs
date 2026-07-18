//! Response / payload types for identity commands (OPN-CORE.md §10.1).

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CharacterInfo {
    pub id: Uuid,
    pub framework_ref: String,
    /// Plain `Option`: serde emits `null` (not absent), so TS is `string | null`.
    pub number: Option<String>,
    pub share_presence: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DeviceInfo {
    pub id: Uuid,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct AppAccountInfo {
    pub id: Uuid,
    pub app_id: String,
    pub handle: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SessionMintResponse {
    pub token: String,
    pub session_id: Uuid,
    pub character: CharacterInfo,
    pub device: DeviceInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct MePayload {
    pub character: CharacterInfo,
    pub device: DeviceInfo,
    pub accounts: Vec<AppAccountInfo>,
    /// The session's `{ app_id: account_id }` map.
    #[ts(type = "Record<string, string>")]
    pub active_accounts: serde_json::Value,
}

// ── channels (OPN-CORE.md §10.2) ────────────────────────────────────────────

/// One message body — the same shape for every channel kind (§10.2); apps
/// interpret it. At least one of `text | media_ids | gif_url` must be present
/// (Core validates at send). `meta` is an opaque app-owned bag.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct MessageBody {
    #[ts(type = "string | null")]
    pub text: Option<String>,
    #[ts(type = "string[] | null")]
    pub media_ids: Option<Vec<Uuid>>,
    #[ts(type = "string | null")]
    pub gif_url: Option<String>,
    #[ts(type = "unknown")]
    pub meta: Option<serde_json::Value>,
}

/// Newest message in a channel, for the `channels.list` snapshot preview.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct MessagePreview {
    #[ts(type = "number")]
    pub seq: i64,
    pub sender: Uuid,
    #[ts(type = "unknown")]
    pub body: serde_json::Value,
    pub at: String,
}

/// One row of the `channels.list` snapshot: the channel, the caller's own
/// watermarks, and a last-message preview.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ChannelSummary {
    pub channel_id: Uuid,
    pub kind: String,
    #[ts(type = "string | null")]
    pub name: Option<String>,
    #[ts(type = "number")]
    pub last_seq: i64,
    #[ts(type = "number")]
    pub last_read_seq: i64,
    #[ts(type = "number")]
    pub last_delivered_seq: i64,
    pub muted: bool,
    pub last_message: Option<MessagePreview>,
}

// ── notify (OPN-CORE.md §10.8) ───────────────────────────────────────────────

/// Semantic urgency of a notification, chosen by the emitting primitive
/// (calls → ring, messages → alert, receipts/likes → silent). Core mandates
/// zero presentation — the shell maps this to toast/badge/ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum NotifyClass {
    Ring,
    Alert,
    Silent,
}

/// One stored notification, read via `GET /v1/notify/inbox` after login.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct InboxItem {
    pub id: Uuid,
    pub app_id: String,
    pub kind: String,
    pub class: NotifyClass,
    #[ts(type = "unknown")]
    pub payload: serde_json::Value,
    #[ts(type = "string | null")]
    pub seen_at: Option<String>,
    pub created_at: String,
}
