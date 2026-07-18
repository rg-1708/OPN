//! channels primitive (OPN-CORE.md §10.2, §8): the product's spine. This
//! module owns request validation and the post-commit fan-out; `store.rs`
//! owns the SQL. Handlers are plain async fns wired into dispatch (Sprint 2
//! chassis).

pub mod store;

use contracts::types::{ChannelSummary, MessageBody, MessageItem, ReceiptKind};
use contracts::{ErrCode, Evt, NotifyClass};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use super::notify::{self, Notification};
use super::Fail;
use crate::gateway::registry::ConnHandle;
use crate::infra::auth::Identity;
use crate::infra::timefmt::rfc3339;
use crate::state::AppState;

/// Resume replay cap (§4.4): a gap larger than this is a cold-load
/// (`channels.resume_overflow`), not a replay.
const RESUME_MAX: i64 = 500;

/// Max emoji length in bytes (§10.2): a small grapheme, not an emoji database.
const EMOJI_MAX_BYTES: usize = 8;

/// Max serialized message body (§10.2). Above this → `too_large`.
const BODY_MAX_BYTES: usize = 8 * 1024;

/// Groups: creator + explicit member list, list capped here (§10.2).
const MEMBERS_MAX: usize = 32;

/// `gif_url` host allowlist (§10.2): external GIF providers, no storage cost.
/// Exact host match (so `eviltenor.com` is rejected; subdomains are listed).
// ponytail: fixed list. Move to config only when a deployment needs custom
// providers — a const recompile is cheap and this rarely changes.
const GIF_HOSTS: &[&str] = &[
    "tenor.com",
    "media.tenor.com",
    "c.tenor.com",
    "giphy.com",
    "media.giphy.com",
    "media0.giphy.com",
    "media1.giphy.com",
    "media2.giphy.com",
    "media3.giphy.com",
    "media4.giphy.com",
];

/// `channels.send` (§8): validate → persist+sequence → ack → fan out.
pub async fn send(
    state: &AppState,
    who: &Identity,
    channel_id: Uuid,
    client_uuid: Uuid,
    body: &MessageBody,
) -> Result<serde_json::Value, Fail> {
    validate_body(body)?;
    // Attachment gate (roadmap Sprint 5 item 6, un-gating Sprint 3 item 3):
    // every attached media id must be a live row owned by the sender.
    if let Some(ids) = body.media_ids.as_ref().filter(|m| !m.is_empty()) {
        if !super::media::all_owned_live(state, who, ids).await? {
            return Err(Fail::Code(ErrCode::Forbidden));
        }
    }
    let body_json = serde_json::to_value(body).map_err(|e| Fail::Internal(e.into()))?;

    let out = store::send_message(
        &state.pg,
        who.world_id,
        who.character_id,
        channel_id,
        client_uuid,
        &body_json,
    )
    .await?;

    // Persist-then-ack (§8): the row is durable and the ack is decided here.
    // An idempotent retry fans out nothing — the original already did.
    if !out.deduped {
        let evt = Evt::ChannelsMessage {
            channel_id,
            message_id: out.message_id,
            seq: out.seq,
            sender: who.character_id,
            body: body_json,
            at: rfc3339(out.created_at),
        };
        // Live fan-out is cheap (local registry, one serialize) → inline.
        crate::gateway::publish(state, who.world_id, &format!("ch:{channel_id}"), &evt).await;

        // Offline members get an inbox row. This can be many DB writes, so it
        // runs off the hot path (§8: fan-out is fire-and-forget post-ack). A
        // crash here loses only the badge; the message row is durable and
        // reaches the member via resume (Sprint 4) on reconnect.
        let members = out.members;
        if members.len() > 1 {
            let state = state.clone();
            let world = who.world_id;
            let sender = who.character_id;
            let message_id = out.message_id;
            let seq = out.seq;
            tokio::spawn(async move {
                for m in members {
                    if m.character_id == sender
                        || state.registry.is_character_online(world, m.character_id)
                    {
                        continue;
                    }
                    let n = Notification {
                        // ponytail: app id fixed to "messages". Per-channel app
                        // binding (guilds/mail route to their own app) lands
                        // with those surfaces.
                        app_id: "messages".into(),
                        kind: "message".into(),
                        class: NotifyClass::Alert,
                        payload: json!({
                            "channel_id": channel_id,
                            "message_id": message_id,
                            "seq": seq,
                        }),
                    };
                    if let Err(e) = notify::route(&state, world, m.character_id, n, m.muted).await {
                        tracing::error!(error = ?e, member = %m.character_id, "notify route failed");
                    }
                }
            });
        }
    }

    Ok(json!({ "message_id": out.message_id, "seq": out.seq }))
}

/// `channels.open_direct` — found-or-create the pair thread to a number.
pub async fn open_direct(
    state: &AppState,
    who: &Identity,
    number: &str,
) -> Result<serde_json::Value, Fail> {
    if number.is_empty() || number.len() > 32 {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    let channel_id = store::open_direct(&state.pg, who.world_id, who.character_id, number).await?;
    Ok(json!({ "channel_id": channel_id }))
}

/// `channels.create` — a group with the caller as first member.
pub async fn create(
    state: &AppState,
    who: &Identity,
    name: Option<String>,
    members: Vec<Uuid>,
) -> Result<serde_json::Value, Fail> {
    if members.len() > MEMBERS_MAX {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    if let Some(n) = &name {
        if n.len() > 128 {
            return Err(Fail::Code(ErrCode::Invalid));
        }
    }
    // Dedupe and drop the creator (auto-added by the store).
    let mut seen = std::collections::HashSet::new();
    let members: Vec<Uuid> = members
        .into_iter()
        .filter(|m| *m != who.character_id && seen.insert(*m))
        .collect();

    let channel_id = store::create_group(
        &state.pg,
        who.world_id,
        who.character_id,
        name.as_deref(),
        &members,
    )
    .await?;
    Ok(json!({ "channel_id": channel_id }))
}

/// `channels.list` — the caller's memberships snapshot.
pub async fn list(state: &AppState, who: &Identity) -> Result<Vec<ChannelSummary>, Fail> {
    store::list_memberships(&state.pg, who.world_id, who.character_id).await
}

/// `sub ch:<id>` authorization (§4.4): membership required.
pub async fn authorize_sub(state: &AppState, who: &Identity, channel_id: Uuid) -> Result<(), Fail> {
    if store::is_member(&state.pg, who.world_id, who.character_id, channel_id).await? {
        Ok(())
    } else {
        Err(Fail::Code(ErrCode::Forbidden))
    }
}

/// Resume replay (§4.4): after the `ch:` sub is authorized and registered,
/// push the gap (`seq > last_seq`) as normal `channels.message` events on this
/// connection *before* the sub ack — so the client's "ack ⇒ caught up" rule
/// holds. Live events may interleave after registration; the client dedups by
/// seq (OPN.md §5). A full 500-row page means the gap is bigger than one
/// replay → a `resume_overflow` tells the client to cold-load via HTTP.
pub async fn resume_replay(
    state: &AppState,
    who: &Identity,
    handle: &std::sync::Arc<ConnHandle>,
    channel_id: Uuid,
    after_seq: i64,
) -> Result<(), Fail> {
    let msgs =
        store::replay_since(&state.pg, who.world_id, channel_id, after_seq, RESUME_MAX).await?;
    let overflow = msgs.len() as i64 == RESUME_MAX;
    let topic = format!("ch:{channel_id}");
    // Backpressured push: the replay can enqueue up to RESUME_MAX (500) durable
    // frames, more than the send queue (default 256), so a plain close-on-full
    // push would kill a healthy client mid-catch-up. `push_to_awaiting` waits
    // for capacity; a `false` means the socket died — stop replaying.
    for m in msgs {
        let evt = Evt::ChannelsMessage {
            channel_id,
            message_id: m.message_id,
            seq: m.seq,
            sender: m.sender,
            body: m.body,
            at: m.at,
        };
        if !state.registry.push_to_awaiting(handle, &topic, &evt).await {
            return Ok(());
        }
    }
    if overflow {
        state
            .registry
            .push_to_awaiting(handle, &topic, &Evt::ChannelsResumeOverflow { channel_id })
            .await;
    }
    Ok(())
}

/// `channels.mark_delivered` / `mark_read` (§10.2): move a watermark, emit a
/// receipt only when it actually advanced (idempotent no-op otherwise).
pub async fn mark(
    state: &AppState,
    who: &Identity,
    channel_id: Uuid,
    kind: ReceiptKind,
    up_to_seq: i64,
) -> Result<(), Fail> {
    let moved = store::mark_watermark(
        &state.pg,
        who.world_id,
        channel_id,
        who.character_id,
        kind,
        up_to_seq,
    )
    .await?;
    if let Some(seq) = moved {
        let evt = Evt::ChannelsReceipt {
            channel_id,
            character_id: who.character_id,
            kind,
            up_to_seq: seq,
            at: rfc3339(OffsetDateTime::now_utc()),
        };
        crate::gateway::publish(state, who.world_id, &format!("ch:{channel_id}"), &evt).await;
    }
    Ok(())
}

/// `channels.typing` (§10.2): ephemeral fan-out, never stored. Membership
/// gated (a non-member cannot type into a channel); the rate bucket, not the
/// server, handles the client's send cadence.
pub async fn typing(state: &AppState, who: &Identity, channel_id: Uuid) -> Result<(), Fail> {
    if !store::is_member(&state.pg, who.world_id, who.character_id, channel_id).await? {
        return Err(Fail::Code(ErrCode::Forbidden));
    }
    let evt = Evt::ChannelsTyping {
        channel_id,
        character_id: who.character_id,
    };
    crate::gateway::publish(state, who.world_id, &format!("ch:{channel_id}"), &evt).await;
    Ok(())
}

/// `channels.react` / `unreact` (§10.2). Emits `channels.reaction` only on a
/// real change (repeat add / absent remove is a silent no-op).
pub async fn react(
    state: &AppState,
    who: &Identity,
    channel_id: Uuid,
    message_id: Uuid,
    emoji: &str,
    add: bool,
) -> Result<(), Fail> {
    if !valid_emoji(emoji) {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    let changed = store::react(
        &state.pg,
        who.world_id,
        channel_id,
        who.character_id,
        message_id,
        emoji,
        add,
    )
    .await?;
    if changed {
        let evt = Evt::ChannelsReaction {
            channel_id,
            message_id,
            character_id: who.character_id,
            emoji: emoji.to_string(),
            added: add,
        };
        crate::gateway::publish(state, who.world_id, &format!("ch:{channel_id}"), &evt).await;
    }
    Ok(())
}

/// `channels.pin` / `unpin` (§10.2). Cap-50 enforced under the channel lock in
/// the store; emits `channels.pin` on a real change.
pub async fn pin(
    state: &AppState,
    who: &Identity,
    channel_id: Uuid,
    message_id: Uuid,
    add: bool,
) -> Result<(), Fail> {
    let changed = store::pin(
        &state.pg,
        who.world_id,
        channel_id,
        who.character_id,
        message_id,
        add,
    )
    .await?;
    if changed {
        let evt = Evt::ChannelsPin {
            channel_id,
            message_id,
            by: who.character_id,
            pinned: add,
        };
        crate::gateway::publish(state, who.world_id, &format!("ch:{channel_id}"), &evt).await;
    }
    Ok(())
}

/// `channels.member_add` / `member_remove` (§10.2, group only). On a real
/// change emits `channels.member`; a removal also drops the removed member's
/// live `ch:` subscription server-side, so they stop receiving at once.
pub async fn member_change(
    state: &AppState,
    who: &Identity,
    channel_id: Uuid,
    target: Uuid,
    add: bool,
) -> Result<(), Fail> {
    let changed = store::member_change(
        &state.pg,
        who.world_id,
        channel_id,
        who.character_id,
        target,
        add,
    )
    .await?;
    if changed {
        let topic = format!("ch:{channel_id}");
        let evt = Evt::ChannelsMember {
            channel_id,
            character_id: target,
            added: add,
        };
        // Publish the removal *before* dropping the member's subscription, so
        // they receive the `added: false` event on their way out; then their
        // socket stops getting this channel's traffic (§10.2).
        crate::gateway::publish(state, who.world_id, &topic, &evt).await;
        if !add {
            state
                .registry
                .drop_character_topic(who.world_id, target, &topic);
        }
    }
    Ok(())
}

/// `GET /v1/channels/:id/messages` history (§6): membership-gated, seq-keyset.
pub async fn history(
    state: &AppState,
    who: &Identity,
    channel_id: Uuid,
    before_seq: Option<i64>,
    limit: i64,
) -> Result<Vec<MessageItem>, Fail> {
    let limit = limit.clamp(1, 100);
    store::history(
        &state.pg,
        who.world_id,
        channel_id,
        who.character_id,
        before_seq,
        limit,
    )
    .await
}

/// A small grapheme allow-check, not an emoji database (§10.2): non-empty,
/// ≤ 8 bytes, no ASCII control or whitespace. Good enough to keep reactions
/// emoji-shaped and cheap; true grapheme-cluster segmentation (ZWJ sequences)
/// would need `unicode-segmentation` — add it only if a real emoji is rejected.
// ponytail: byte-cap + no-control. Upgrade to grapheme segmentation if the
// allow-set proves too tight for real emoji.
fn valid_emoji(emoji: &str) -> bool {
    !emoji.is_empty()
        && emoji.len() <= EMOJI_MAX_BYTES
        && !emoji
            .chars()
            .any(|c| c.is_ascii_control() || c.is_whitespace())
}

fn validate_body(body: &MessageBody) -> Result<(), Fail> {
    let has_text = body.text.as_deref().is_some_and(|t| !t.is_empty());
    let has_media = body.media_ids.as_ref().is_some_and(|m| !m.is_empty());
    let has_gif = body.gif_url.as_deref().is_some_and(|g| !g.is_empty());
    if !(has_text || has_media || has_gif) {
        return Err(Fail::Code(ErrCode::Invalid));
    }
    // Media ownership is validated asynchronously in `send` (needs the DB); this
    // sync check is shape/size only. See `media::all_owned_live`.

    if let Some(url) = body.gif_url.as_deref() {
        if !url.is_empty() && !gif_host_allowed(url) {
            return Err(Fail::Code(ErrCode::Invalid));
        }
    }

    let size = serde_json::to_vec(body)
        .map_err(|e| Fail::Internal(e.into()))?
        .len();
    if size > BODY_MAX_BYTES {
        return Err(Fail::Code(ErrCode::TooLarge));
    }
    Ok(())
}

/// `https://<host>[:port]/...`, host matched exactly against the allowlist.
/// No `url` crate in core — a tiny hand-parse is enough and dependency-free.
fn gif_host_allowed(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split(['/', ':', '?', '#']).next().unwrap_or("");
    GIF_HOSTS.iter().any(|h| host.eq_ignore_ascii_case(h))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(text: Option<&str>, gif: Option<&str>, media: Option<Vec<Uuid>>) -> MessageBody {
        MessageBody {
            text: text.map(str::to_string),
            media_ids: media,
            gif_url: gif.map(str::to_string),
            meta: None,
        }
    }

    #[test]
    fn empty_body_rejected() {
        assert!(matches!(
            validate_body(&body(None, None, None)),
            Err(Fail::Code(ErrCode::Invalid))
        ));
        assert!(matches!(
            validate_body(&body(Some(""), None, None)),
            Err(Fail::Code(ErrCode::Invalid))
        ));
    }

    #[test]
    fn text_ok() {
        assert!(validate_body(&body(Some("hi"), None, None)).is_ok());
    }

    #[test]
    fn media_passes_shape_validation() {
        // Sprint 5: a media-only body is now shape-valid; ownership (live +
        // owned-by-sender) is checked asynchronously in `send`, not here.
        assert!(validate_body(&body(None, None, Some(vec![Uuid::now_v7()]))).is_ok());
    }

    #[test]
    fn gif_host_allowlist() {
        assert!(gif_host_allowed("https://media.tenor.com/abc.gif"));
        assert!(gif_host_allowed("https://TENOR.com/x")); // case-insensitive
        assert!(!gif_host_allowed("https://eviltenor.com/x")); // exact host only
        assert!(!gif_host_allowed("http://media.tenor.com/x")); // https only
        assert!(!gif_host_allowed("https://evil.com/media.tenor.com"));
        assert!(validate_body(&body(None, Some("https://media.tenor.com/a.gif"), None)).is_ok());
        assert!(matches!(
            validate_body(&body(None, Some("https://evil.com/a.gif"), None)),
            Err(Fail::Code(ErrCode::Invalid))
        ));
    }

    #[test]
    fn oversize_body_rejected() {
        let big = "x".repeat(9 * 1024);
        assert!(matches!(
            validate_body(&body(Some(&big), None, None)),
            Err(Fail::Code(ErrCode::TooLarge))
        ));
    }
}
