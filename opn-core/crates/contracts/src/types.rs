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
    /// For `dm` channels: the peer's character id, so the client can subscribe
    /// `presence:<id>` without waiting for the peer to emit traffic (contract
    /// gap #4). `null` for groups AND for a DM where the peer has not yet
    /// emitted a message — revealing it before then would let a caller map a
    /// number to a character just by opening a DM, which the §10.7 number-opaque
    /// model forbids. Once the peer has spoken their id is already observable in
    /// every message they send, so this leaks nothing new.
    #[ts(type = "string | null")]
    pub peer_character_id: Option<Uuid>,
    /// For `dm` channels: the peer's dialable number, so the DM header can render
    /// call buttons after a reload (contract gap #10). `null` for groups (and for
    /// a peer with no assigned number). Numbers are mutually known to DM parties
    /// — the same number already rides the ring payload as caller-ID — so this
    /// reveals nothing the directory keeps opaque.
    #[ts(type = "string | null")]
    pub peer_number: Option<String>,
}

/// One reaction on a history row (§10.2): the emoji and who reacted. Same
/// granularity as the live `channels.reaction` event, so a cold-load rebuilds
/// the exact same reaction map the live stream would.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ReactionItem {
    pub emoji: String,
    pub character_id: Uuid,
}

/// One message row in a `GET /v1/channels/:id/messages` history page (§6).
/// Seq-keyed (not the time cursor) — seq is already public in this contract.
/// `pinned` and `reactions` are the durable channel state a cold-load must
/// carry so a reload doesn't lose reactions/pins that only existed as live
/// events this session (contract gap #2).
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
    /// Is this message currently pinned in its channel (§10.2).
    pub pinned: bool,
    /// Every reaction on this message (§10.2), so the client renders reaction
    /// state on a fresh load without replaying the live event stream.
    pub reactions: Vec<ReactionItem>,
}

/// One member of a channel (`channels.members`, §10.2): character id + when they
/// joined. Membership-gated read; carries no phone number (the §10.7 privacy
/// boundary), only the character id a co-member already sees in every message,
/// receipt, and `channels.member` event.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ChannelMember {
    pub character_id: Uuid,
    pub joined_at: String,
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

// ── calls (OPN-CORE.md §10.4) ─────────────────────────────────────────────────

/// Call medium (§10.4). `calls.start`'s `video: bool` maps here — `false` →
/// voice, `true` → video. Voice audio always rides pma-voice; WebRTC carries
/// video only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum CallKind {
    Voice,
    Video,
}

/// Session lifecycle (§10.4) — the FSM's session states. `ringing` until the
/// first accept, `active` while ≥ 1 participant is joined, `ended` is terminal
/// (nothing leaves it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum CallSessionState {
    Ringing,
    Active,
    Ended,
}

/// Per-participant lifecycle (§10.4) — the FSM's participant states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum CallParticipantState {
    Ringing,
    Joined,
    Declined,
    Left,
}

/// One participant in a `calls.state` snapshot (§10.4): character id + state.
/// Device and timestamps stay server-side — opaque to the peer.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CallParticipant {
    pub character_id: Uuid,
    pub state: CallParticipantState,
}

/// Call media topology (opn-group-calls.md G0). `p2p` — 1:1 calls, media flows
/// peer-to-peer and Core relays only signaling (Sprint 6). `sfu` — group calls,
/// media forwards through the LiveKit sidecar. Carried on every call snapshot
/// from day one so a future topology change never breaks a pinned client
/// (additive-only, contracts-semver.md). `p2p` is the default, so an old
/// snapshot without the field deserializes unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum Topology {
    #[default]
    P2p,
    Sfu,
}

/// `calls.group.join` ack (opn-group-calls.md G0): a short-lived LiveKit access
/// token plus the SFU URL to dial. The client connects to the SFU directly with
/// these — media bytes never pass through Core. Token TTL is short (≤ 60 s to
/// connect; the LiveKit session survives token expiry). `expires_at` is RFC 3339.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct GroupJoinAck {
    pub sfu_url: String,
    pub token: String,
    pub expires_at: String,
}

// ── ledger (OPN-CORE.md §10.5) ────────────────────────────────────────────────

/// One transfer in a `GET /v1/ledger/history` page (§10.5). Raw `from`/`to`
/// account ids + amount; the client owns its own account ids and renders
/// direction from them. `kind` is `transfer` (a `ledger.transfer`) or `capture`
/// (a settled hold). Never carries a balance — history is the journal, not a
/// running total.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TransferItem {
    pub id: Uuid,
    pub from_account: Uuid,
    pub to_account: Uuid,
    #[ts(type = "number")]
    pub amount: i64,
    pub kind: String,
    pub created_at: String,
}

// ── feed (OPN-CORE.md §10.3) ──────────────────────────────────────────────────

/// What a `feed.activity` advisory reports (§10.3): a new post, a like, or a
/// comment. Clients viewing the feed refresh on any of these; the closed set
/// keeps the advisory typed rather than a free-form string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum FeedActivityKind {
    Post,
    Like,
    /// A like was removed (contract gap #7): lets a live viewer decrement its
    /// optimistic `like_count` instead of only ever seeing it rise between
    /// reloads. Additive kind; a client that doesn't know it just refreshes.
    Unlike,
    Comment,
    /// A post was deleted (contract gap #7): lets a live viewer drop the post
    /// instead of rendering a stale row until the next page fetch.
    Delete,
}

/// One post in a feed read page (home/profile/hashtag timelines, post detail;
/// §10.3, Sprint 8 part B). `body` is the opaque app-owned doc Core caps but
/// never interprets; `media_ids` were validated owned+live at write time (a
/// later-deleted media renders missing). `like_count`/`comment_count` are the
/// denormalized exact counters. Newest-first on the shared cursor idiom (CDR-7).
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct PostItem {
    pub id: Uuid,
    pub app_id: String,
    pub author_account: Uuid,
    /// The author's public handle (contract gap #8), denormalized at read time
    /// so a client renders a name instead of a truncated account uuid. Handles
    /// are already public within an app (`UNIQUE (world_id, app_id, handle)`).
    pub author_handle: String,
    #[ts(type = "unknown")]
    pub body: serde_json::Value,
    #[ts(type = "string[]")]
    pub media_ids: Vec<Uuid>,
    #[ts(type = "number")]
    pub like_count: i64,
    #[ts(type = "number")]
    pub comment_count: i64,
    /// Viewer-relative (contract gap #6): did the caller's active account for
    /// this app like this post — so the like button renders its true state after
    /// a reload. `false` when the caller isn't logged into the app.
    pub liked_by_viewer: bool,
    /// Viewer-relative (contract gap #6): does the caller's active account follow
    /// this post's author. `false` for own posts and when not logged in.
    pub author_following: bool,
    pub created_at: String,
}

/// One comment in a post-detail page (§10.3, Sprint 8 part B). `body` is opaque,
/// size-capped like a post; newest-first on the cursor idiom.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CommentItem {
    pub id: Uuid,
    pub post_id: Uuid,
    pub author_account: Uuid,
    /// The comment author's public handle (contract gap #8), same rationale as
    /// `PostItem::author_handle`.
    pub author_handle: String,
    #[ts(type = "unknown")]
    pub body: serde_json::Value,
    pub created_at: String,
}

// ── tenant link (OPN-CORE.md §5) ──────────────────────────────────────────────

/// Voice-target action on the tenant link (§5, §10.4): `set_targets` names the
/// characters whose game-voice Core wants bound for a call; `clear` tears them
/// down when the call ends. Down-only (Core → FXServer), tenant-scoped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum VoiceAction {
    SetTargets,
    Clear,
}

/// Link hello (§5): the first frame the FXServer gateway resource sends after
/// opening `wss://core/link`. Core logs the pair and refuses only known-broken
/// combos — the field is the seam, enforcement slots in later without a protocol
/// change (closes §17 Q4). Up-direction carries nothing else.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct LinkHello {
    pub resource_version: String,
    pub contracts_version: String,
}

/// One active call in the `GET /v1/tenants/self/calls/active` re-sync (§5): the
/// tenant link reads these on (re)connect to rebuild voice targets after a drop.
/// Same shape as a `calls.state` snapshot minus the ICE config.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ActiveCall {
    pub call_id: Uuid,
    pub kind: CallKind,
    pub state: CallSessionState,
    pub participants: Vec<CallParticipant>,
}
