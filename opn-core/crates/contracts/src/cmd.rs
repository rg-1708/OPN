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
    /// List a channel's members (§10.2, contract gap #3). Membership-gated
    /// (`forbidden` for a non-member). Ack `ChannelMember[]` ordered by
    /// `joined_at` — character ids + join times only, never phone numbers.
    #[serde(rename = "channels.members")]
    ChannelsMembers {
        channel_id: Uuid,
    },
    /// Set the caller's own mute flag on a channel (§10.2, contract gap #3).
    /// Idempotent; `forbidden` if the caller isn't a member. Drives the notify
    /// suppression split (a muted channel downgrades alerts to silent).
    #[serde(rename = "channels.set_muted")]
    ChannelsSetMuted {
        channel_id: Uuid,
        muted: bool,
    },

    // ── servers (§10.2a, contract gap #13) ───────────────────────────────
    /// Create a server (channel container). Caller becomes owner + first
    /// member. `banner_media_id` must reference live media when present.
    /// Ack `{ server_id }`.
    #[serde(rename = "servers.create")]
    ServersCreate {
        name: String,
        #[ts(type = "string | null")]
        banner_media_id: Option<Uuid>,
    },
    /// Snapshot of the caller's server memberships. Ack `ServerSummary[]`.
    /// The per-server channel tree comes from `channels.list` (rows with a
    /// matching `server_id`, grouped by `category`, ordered by `position`).
    #[serde(rename = "servers.list")]
    ServersList,
    /// Add a member (owner only; `forbidden` otherwise). Idempotent. The new
    /// member gains membership in every channel of the server; they learn of
    /// it via a `notify.event` (app_id `servers`, kind `server_member_added`).
    #[serde(rename = "servers.member_add")]
    ServersMemberAdd {
        server_id: Uuid,
        character_id: Uuid,
    },
    /// Remove a member: the owner may remove anyone else, anyone may remove
    /// themself (leave). The owner cannot leave their own server (`conflict`;
    /// ownership transfer is out of scope). Drops the member from every
    /// channel of the server and their live subscriptions to them.
    #[serde(rename = "servers.member_remove")]
    ServersMemberRemove {
        server_id: Uuid,
        character_id: Uuid,
    },
    /// Create a channel inside a server (owner only). `kind` is `group`
    /// (text) or `voice` (a marker kind: audio itself rides the existing
    /// group-call primitive; a voice channel still carries messages).
    /// `category`/`position` only drive the client's tree. Every current
    /// server member becomes a channel member. Ack `{ channel_id }`.
    #[serde(rename = "servers.channel_create")]
    ServersChannelCreate {
        server_id: Uuid,
        name: String,
        kind: String,
        #[ts(type = "string | null")]
        category: Option<String>,
        position: i32,
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

    // ── group calls (opn-group-calls.md G0) ──────────────────────────────
    /// Create a group call (opn-group-calls.md G0). `label` is an optional
    /// human-readable name; `max_participants`, if present, caps the room within
    /// the server limit (config default 32 — a larger value is clamped, not
    /// rejected). Returns `{ call_id }`. Group media rides the SFU (`topology:
    /// "sfu"`), not the P2P path.
    #[serde(rename = "calls.group.create")]
    CallsGroupCreate {
        #[ts(type = "string | null")]
        label: Option<String>,
        #[ts(type = "number | null")]
        max_participants: Option<i64>,
    },
    /// Join a group call (opn-group-calls.md G0): Core checks membership and mints
    /// a short-lived LiveKit token. Ack `GroupJoinAck { sfu_url, token, expires_at }`
    /// — the client dials the SFU directly (media never transits Core). A full
    /// room → `conflict`. v1 is open-join within the world: any character
    /// holding the `call_id` may join (invites/allowlists are gated).
    #[serde(rename = "calls.group.join")]
    CallsGroupJoin {
        call_id: Uuid,
    },
    /// Leave a group call (opn-group-calls.md G0). The last leave ends the session.
    #[serde(rename = "calls.group.leave")]
    CallsGroupLeave {
        call_id: Uuid,
    },
    /// End a group call for everyone (opn-group-calls.md G0): creator/privileged
    /// only. Tears the room down; a non-privileged caller → `forbidden`.
    #[serde(rename = "calls.group.end")]
    CallsGroupEnd {
        call_id: Uuid,
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

    // ── feed (§10.3) ─────────────────────────────────────────────────────
    /// Author a post as the caller's active account for `app_id` (§10.3). `body`
    /// is an opaque app-owned doc (≤ 4 KB); `media_ids` must be live media owned
    /// by the caller. Hashtags are parsed server-side from `body.text`. Not
    /// logged into the app → `forbidden`. Ack `{ post_id }`.
    #[serde(rename = "feed.post")]
    FeedPost {
        app_id: String,
        #[ts(type = "unknown")]
        body: serde_json::Value,
        media_ids: Vec<Uuid>,
    },
    /// Delete one of the caller's own posts (§10.3): author-only, hard delete,
    /// cascading its likes/comments/hashtags in one tx.
    #[serde(rename = "feed.delete")]
    FeedDelete {
        app_id: String,
        post_id: Uuid,
    },
    /// Like a post (§10.3): idempotent, bumps `like_count` in-tx and (on a real
    /// change) advises the feed + silently notifies the author.
    #[serde(rename = "feed.like")]
    FeedLike {
        app_id: String,
        post_id: Uuid,
    },
    /// Remove the caller's like (§10.3): idempotent, decrements `like_count`.
    #[serde(rename = "feed.unlike")]
    FeedUnlike {
        app_id: String,
        post_id: Uuid,
    },
    /// Comment on a post (§10.3): `body` ≤ 4 KB, bumps `comment_count` in-tx.
    /// Ack `{ comment_id }`.
    #[serde(rename = "feed.comment")]
    FeedComment {
        app_id: String,
        post_id: Uuid,
        #[ts(type = "unknown")]
        body: serde_json::Value,
    },
    /// Follow another account in the app (§10.3): idempotent; self-follow is
    /// `invalid`, an unknown target is `not_found`.
    #[serde(rename = "feed.follow")]
    FeedFollow {
        app_id: String,
        account_id: Uuid,
    },
    /// Unfollow an account (§10.3): idempotent.
    #[serde(rename = "feed.unfollow")]
    FeedUnfollow {
        app_id: String,
        account_id: Uuid,
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
