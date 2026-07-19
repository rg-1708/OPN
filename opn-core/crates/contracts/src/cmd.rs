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

    // ── directory (§10.7) ────────────────────────────────────────────────
    /// Create or update a contact, keyed on `number` (§10.7). Upsert: a repeat
    /// number replaces the display fields. `avatar_media`, if present, must be a
    /// live media owned by the caller (`invalid` otherwise).
    #[serde(rename = "directory.contact_upsert")]
    DirectoryContactUpsert {
        number: String,
        display_name: String,
        #[ts(type = "string | null")]
        avatar_media: Option<Uuid>,
        #[ts(type = "unknown")]
        meta: Option<serde_json::Value>,
    },
    /// Delete one of the caller's contacts by number.
    #[serde(rename = "directory.contact_delete")]
    DirectoryContactDelete {
        number: String,
    },
    /// Snapshot the caller's contact book (§10.7).
    #[serde(rename = "directory.contacts")]
    DirectoryContacts,
    /// Block a number (§10.7). Idempotent; enforced at action points, not here.
    #[serde(rename = "directory.block")]
    DirectoryBlock {
        number: String,
    },
    /// Unblock a number (idempotent).
    #[serde(rename = "directory.unblock")]
    DirectoryUnblock {
        number: String,
    },
    /// The caller's blocked numbers (so the client can render an unblock list).
    #[serde(rename = "directory.blocks")]
    DirectoryBlocks,
    /// Opaque number resolution (§10.7): `{ reachable, number, display_name }`,
    /// never a character id. A blocked pair reads exactly like an unknown number.
    #[serde(rename = "directory.resolve")]
    DirectoryResolve {
        number: String,
    },
    /// Post a listing (§10.7). `ttl_secs`, if present, sets the expiry the
    /// janitor sweeps by; absent = never expires.
    #[serde(rename = "directory.listing_create")]
    DirectoryListingCreate {
        app_id: String,
        kind: String,
        title: String,
        #[ts(type = "unknown")]
        body: Option<serde_json::Value>,
        contact_number: String,
        #[ts(type = "number | null")]
        ttl_secs: Option<i64>,
    },
    /// Delete one of the caller's own listings.
    #[serde(rename = "directory.listing_delete")]
    DirectoryListingDelete {
        id: Uuid,
    },
    /// A page of active listings for an app (§10.7), newest-first on the cursor
    /// idiom (CDR-7).
    #[serde(rename = "directory.listings")]
    DirectoryListings {
        app_id: String,
        #[ts(type = "string | null")]
        cursor: Option<String>,
        #[ts(type = "number | null")]
        limit: Option<i64>,
    },

    // ── calls (§10.4) ────────────────────────────────────────────────────
    /// Start a call to a number (§10.4). Resolves through the directory seam
    /// (blocked/unknown → `not_found`, privacy); a busy callee → `conflict`.
    /// `video: false` is a voice call, `true` a video call. Rings the callee via
    /// notify (the dialer needs no standing sub) and returns `{ call_id }`.
    #[serde(rename = "calls.start")]
    CallsStart {
        callee_number: String,
        video: bool,
    },
    /// Accept a ringing call (§10.4): the caller's participant → joined, session
    /// → active. Illegal from any non-ringing state → `conflict`.
    #[serde(rename = "calls.accept")]
    CallsAccept {
        call_id: Uuid,
    },
    /// Decline a ringing call (§10.4). Ends the session when no one else is
    /// still ringing or joined.
    #[serde(rename = "calls.decline")]
    CallsDecline {
        call_id: Uuid,
    },
    /// Hang up a joined call (§10.4). The last hangup ends the session.
    #[serde(rename = "calls.hangup")]
    CallsHangup {
        call_id: Uuid,
    },
    /// Opaque WebRTC signaling relay (§10.4): forwarded verbatim to `to` on
    /// `call:<id>`, never inspected or stored. Sender and `to` must both be
    /// active participants of a ringing/active call. `payload` ≤ 16 KB.
    #[serde(rename = "calls.signal")]
    CallsSignal {
        call_id: Uuid,
        to: Uuid,
        #[ts(type = "unknown")]
        payload: serde_json::Value,
    },

    // ── ledger (§10.5) ───────────────────────────────────────────────────
    /// Move `amount` from `from_account` to `to_account` (§10.5). `from_account`
    /// must be the caller's; `client_uuid` is the idempotency key (a retry
    /// returns the original ack and moves nothing). Ack `{ transfer_id, balance }`
    /// where `balance` is the source's new balance.
    #[serde(rename = "ledger.transfer")]
    LedgerTransfer {
        from_account: Uuid,
        to_account: Uuid,
        #[ts(type = "number")]
        amount: i64,
        client_uuid: Uuid,
    },
    /// Reserve `amount` on the caller's own `account` for `expires_in_secs`
    /// (§10.5): held funds don't move but are excluded from available balance
    /// until captured or released. Ack `{ hold_id }`.
    #[serde(rename = "ledger.hold")]
    LedgerHold {
        account: Uuid,
        #[ts(type = "number")]
        amount: i64,
        #[ts(type = "number")]
        expires_in_secs: i64,
    },
    /// Settle a hold to a destination (§10.5): held → captured, moving the amount
    /// from the holding account to `to`. Ack `{ transfer_id }`.
    #[serde(rename = "ledger.capture")]
    LedgerCapture {
        hold_id: Uuid,
        to: Uuid,
    },
    /// Free a hold without moving money (§10.5): held → released.
    #[serde(rename = "ledger.release")]
    LedgerRelease {
        hold_id: Uuid,
    },
    /// Start a framework withdraw (§10.5, OPN.md §14.2), leg 1 of 2: reserve
    /// `amount` on the caller's wallet with a hold AND open a `pending_confirm`
    /// exchange. Ack `{ exchange_id }` — the client relays it to the bridge, which
    /// credits the framework bank and calls `withdraw_confirm` to settle the hold
    /// to the tenant `system` account. Unconfirmed → the hold expires and the
    /// exchange auto-expires (janitor).
    #[serde(rename = "ledger.withdraw")]
    LedgerWithdraw {
        #[ts(type = "number")]
        amount: i64,
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
