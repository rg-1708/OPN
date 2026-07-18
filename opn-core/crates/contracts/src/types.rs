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
    /// For `dm` channels: the other party's last-seen (RFC 3339), gated on
    /// their `share_presence` — `null` for groups, or when they don't share
    /// (§10.2). Read-time honored, so a presence toggle takes effect on the
    /// next list.
    #[ts(type = "string | null")]
    pub last_seen_at: Option<String>,
}

/// One message row in a `GET /v1/channels/:id/messages` history page (§6).
/// Seq-keyed (not the time cursor) — seq is already public in this contract.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct MessageItem {
    pub message_id: Uuid,
    #[ts(type = "number")]
    pub seq: i64,
    pub sender: Uuid,
    #[ts(type = "unknown")]
    pub body: serde_json::Value,
    pub at: String,
}

/// Which watermark a `channels.receipt` event carries (§10.2). `delivered` is
/// device-received; `read` is user-read. Both monotonic, both watermark-only
/// (never per-message rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ReceiptKind {
    Delivered,
    Read,
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

// ── media (OPN-CORE.md §10.6) ────────────────────────────────────────────────

/// Upload kind (§10.6): fixes the MIME allowlist, the size cap, and whether a
/// thumbnail target is issued (photo/video yes, audio no).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum MediaKind {
    Photo,
    Video,
    Audio,
}

/// One presigned S3 POST target (§10.6). The client POSTs a multipart form to
/// `url` with every entry of `fields` plus a trailing `file` part — nothing
/// else. The cap is MinIO's, enforced by the policy inside `fields`, so a
/// cheating client cannot lift it.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct UploadTarget {
    /// `original` or `thumb`.
    pub role: String,
    pub url: String,
    #[ts(type = "Record<string, string>")]
    pub fields: serde_json::Value,
}

/// `media.request_upload` ack (§10.6): the new media id and one or two POST
/// targets (thumb present for photo/video).
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct UploadTicket {
    pub media_id: Uuid,
    pub targets: Vec<UploadTarget>,
}

/// One gallery row (`GET /v1/media`, §10.6). `url`/`thumb_url` are short-lived
/// presigned GETs — the client fetches bytes straight from S3, never via Core.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct MediaItem {
    pub media_id: Uuid,
    pub kind: MediaKind,
    pub mime: String,
    #[ts(type = "number")]
    pub bytes: i64,
    pub url: String,
    #[ts(type = "string | null")]
    pub thumb_url: Option<String>,
    pub created_at: String,
}

// ── directory (OPN-CORE.md §10.7) ─────────────────────────────────────────────

/// One row of the caller's private contact book (`directory.contacts`).
/// Contacts point at raw numbers; a number resolves to a character only at
/// action time, so this carries no character id.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ContactItem {
    pub number: String,
    pub display_name: String,
    /// A live media owned by the caller, validated at write time (§10.7).
    #[ts(type = "string | null")]
    pub avatar_media: Option<Uuid>,
    #[ts(type = "unknown")]
    pub meta: serde_json::Value,
    pub created_at: String,
}

/// `directory.resolve` result (§10.7): opaque routing. `reachable` is the only
/// signal — false for both an unknown number and a blocked pair, so a block is
/// indistinguishable from no-such-number (privacy). `display_name` is the
/// caller's OWN saved label for the number (their data, never the target's), so
/// it leaks nothing about the character behind the number. No character id ever
/// crosses this wire.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ResolveResult {
    pub reachable: bool,
    pub number: String,
    #[ts(type = "string | null")]
    pub display_name: Option<String>,
}

/// One listing (`directory.listings`, §10.7): an app-scoped ad/posting with an
/// optional TTL. `contact_number` is the number a reader would call — free-form,
/// not a resolved character.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ListingItem {
    pub id: Uuid,
    pub app_id: String,
    pub kind: String,
    pub title: String,
    #[ts(type = "unknown")]
    pub body: serde_json::Value,
    pub contact_number: String,
    pub created_at: String,
    #[ts(type = "string | null")]
    pub expires_at: Option<String>,
}
