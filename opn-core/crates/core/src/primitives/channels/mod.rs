//! channels primitive (OPN-CORE.md §10.2, §8): the product's spine. This
//! module owns request validation and the post-commit fan-out; `store.rs`
//! owns the SQL. Handlers are plain async fns wired into dispatch (Sprint 2
//! chassis).

pub mod store;

use contracts::types::{ChannelSummary, MessageBody};
use contracts::{ErrCode, Evt, NotifyClass};
use serde_json::json;
use uuid::Uuid;

use super::notify::{self, Notification};
use super::Fail;
use crate::infra::auth::Identity;
use crate::infra::timefmt::rfc3339;
use crate::state::AppState;

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

fn validate_body(body: &MessageBody) -> Result<(), Fail> {
    let has_text = body.text.as_deref().is_some_and(|t| !t.is_empty());
    let has_media = body.media_ids.as_ref().is_some_and(|m| !m.is_empty());
    let has_gif = body.gif_url.as_deref().is_some_and(|g| !g.is_empty());
    if !(has_text || has_media || has_gif) {
        return Err(Fail::Code(ErrCode::Invalid));
    }

    // Attachment authz gate (roadmap Sprint 3 item 3): the media table does
    // not exist until Sprint 5, so no media id can be a live, owned row —
    // any attachment is unverifiable and therefore forbidden. Sprint 5 item 6
    // un-gates this into the real owned+live count check.
    if has_media {
        return Err(Fail::Code(ErrCode::Forbidden));
    }

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
    fn media_gated_off_until_sprint5() {
        assert!(matches!(
            validate_body(&body(None, None, Some(vec![Uuid::now_v7()]))),
            Err(Fail::Code(ErrCode::Forbidden))
        ));
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
