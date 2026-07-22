//! Sequential per-connection dispatch (§7, CDR-5): parse → rate limit →
//! match → handler → ack. Handlers are plain async fns in `primitives`; this
//! module owns the wire ack, the span, and the metrics.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use contracts::types::ReceiptKind;
use contracts::{Cmd, ErrBody, ErrCode, ServerMsg};
use metrics::{counter, histogram};
use serde_json::json;
use tracing::Instrument;

use super::registry::ConnHandle;
use super::topic::TopicKind;
use crate::infra::auth::mint_jwt;
use crate::infra::cursor;
use crate::infra::ratelimit::class_of;
use crate::primitives::{calls, channels, directory, feed, identity, ledger, media, notify, Fail};
use crate::state::AppState;

/// Handles one parsed frame, returns the ack. Never panics, never closes —
/// protocol errors become acks (§7); closing is the lifecycle's job.
pub async fn dispatch(state: &AppState, handle: &Arc<ConnHandle>, id: u64, cmd: Cmd) -> ServerMsg {
    let cmd_name = wire_name(&cmd);
    let who = &handle.identity;

    if let Err(retry_after_ms) = state.limits.check(who.character_id, class_of(&cmd)) {
        counter!("opn_commands_total", "cmd" => cmd_name, "outcome" => "rate_limited").increment(1);
        return ServerMsg::Ack {
            reply_to: id,
            ok: false,
            payload: Some(json!({ "retry_after_ms": retry_after_ms })),
            err: Some(ErrBody {
                code: ErrCode::RateLimited,
                msg: "rate limited".into(),
            }),
        };
    }

    let span = tracing::info_span!(
        "cmd",
        cmd = cmd_name,
        tenant = %who.tenant_id,
        world = %who.world_id,
        char = %who.character_id,
    );
    let start = Instant::now();
    let result = run(state, handle, cmd).instrument(span).await;
    histogram!("opn_command_seconds", "cmd" => cmd_name).record(start.elapsed().as_secs_f64());

    match result {
        Ok(payload) => {
            counter!("opn_commands_total", "cmd" => cmd_name, "outcome" => "ok").increment(1);
            ServerMsg::Ack {
                reply_to: id,
                ok: true,
                payload,
                err: None,
            }
        }
        Err(Fail::Code(code)) => {
            counter!("opn_commands_total", "cmd" => cmd_name, "outcome" => "err").increment(1);
            ServerMsg::Ack {
                reply_to: id,
                ok: false,
                payload: None,
                err: Some(ErrBody {
                    code,
                    msg: String::new(),
                }),
            }
        }
        Err(Fail::Internal(e)) => {
            // Detail stays in the log, never on the wire (§7).
            tracing::error!(error = %e, cmd = cmd_name, "handler internal error");
            counter!("opn_commands_total", "cmd" => cmd_name, "outcome" => "internal").increment(1);
            ServerMsg::Ack {
                reply_to: id,
                ok: false,
                payload: None,
                err: Some(ErrBody {
                    code: ErrCode::Internal,
                    msg: String::new(),
                }),
            }
        }
    }
}

async fn run(
    state: &AppState,
    handle: &Arc<ConnHandle>,
    cmd: Cmd,
) -> Result<Option<serde_json::Value>, Fail> {
    let who = &handle.identity;
    match cmd {
        // Already authenticated — one auth per connection (§4.1).
        Cmd::Auth { .. } => Err(Fail::Code(ErrCode::Conflict)),

        Cmd::AuthRefresh => {
            // Re-check revocation and bump the session in one guarded UPDATE;
            // zero rows = revoked/expired underneath us (§11).
            let mut tx = crate::infra::db::world_tx(&state.pg, who.world_id).await?;
            let bumped: Option<i32> = sqlx::query_scalar(
                "UPDATE sessions SET expires_at = now() + make_interval(secs => $2) \
                 WHERE id = $1 AND revoked_at IS NULL AND expires_at > now() RETURNING 1",
            )
            .bind(who.session_id)
            .bind(state.cfg.session_ttl_secs as f64)
            .fetch_optional(&mut *tx)
            .await?;
            tx.commit().await?;
            if bumped.is_none() {
                return Err(Fail::Code(ErrCode::Unauthorized));
            }
            let token = mint_jwt(&state.cfg.jwt_secret, who).map_err(Fail::Internal)?;
            Ok(Some(json!({ "token": token })))
        }

        Cmd::Sub { topic, last_seq } => {
            let Some(kind) = TopicKind::parse(&topic) else {
                return Err(Fail::Code(ErrCode::Invalid));
            };
            match kind {
                TopicKind::Notify(device) => {
                    // Own device only (§4.4).
                    if device != who.device_id {
                        return Err(Fail::Code(ErrCode::Forbidden));
                    }
                    state.registry.subscribe(&topic, handle);
                    Ok(None)
                }
                TopicKind::Presence(character) => {
                    let snap = super::presence::snapshot(state, who.world_id, character).await?;
                    state.registry.subscribe(&topic, handle);
                    // Snapshot before the ack (§4.4): ack received ⇒
                    // snapshot delivered.
                    state.registry.push_to(handle, &topic, &snap);
                    Ok(None)
                }
                TopicKind::Ch(channel_id) => {
                    // Membership authz (§4.4), then register, then — if the
                    // client sent a watermark — replay the gap before the ack
                    // (§4.4 snapshot-before-ack). Register first so no live
                    // event between replay and ack is lost.
                    channels::authorize_sub(state, who, channel_id).await?;
                    state.registry.subscribe(&topic, handle);
                    if let Some(after) = last_seq {
                        channels::resume_replay(state, who, handle, channel_id, after).await?;
                    }
                    Ok(None)
                }
                TopicKind::Call(call_id) => {
                    // Participant-only (§10.4). Subscribe-first (like the `ch:`
                    // arm, not presence): authorize → register → snapshot → push,
                    // so a live `calls.state` transition landing after
                    // registration is delivered, not lost — a durable full-state
                    // event has no seq to heal a miss. Residual: a transition
                    // committing between the snapshot read and its push can be
                    // reordered ahead of the stale snapshot (a transient regress
                    // the next transition heals; a `left`/`ended` terminal state
                    // is sticky client-side). A monotonic `version` on
                    // `calls.state` would close it fully — deferred (§10.4 chose
                    // seqless snapshots).
                    calls::authorize_sub(state, who, call_id).await?;
                    state.registry.subscribe(&topic, handle);
                    let snap = calls::snapshot(state, who, call_id).await?;
                    state.registry.push_to(handle, &topic, &snap);
                    Ok(None)
                }
                TopicKind::Feed(app_id) => {
                    // Any character with an app account for the app may watch
                    // (§10.3). Advisory-only stream, so no snapshot-on-sub.
                    feed::authorize_sub(state, who, &app_id).await?;
                    state.registry.subscribe(&topic, handle);
                    Ok(None)
                }
            }
        }

        Cmd::Unsub { topic } => {
            state.registry.unsubscribe(&topic, handle);
            Ok(None)
        }

        Cmd::IdentityMe => {
            let me = identity::me(&state.pg, who).await?;
            Ok(Some(serde_json::to_value(me).map_err(anyhow::Error::from)?))
        }
        Cmd::IdentityAppLogin { app_id, account_id } => {
            identity::app_login(&state.pg, who, &app_id, account_id).await?;
            Ok(None)
        }
        Cmd::IdentityGetSettings { scope } => {
            let doc = identity::get_settings(&state.pg, who, scope).await?;
            Ok(Some(doc))
        }
        Cmd::IdentitySetSettings { scope, patch } => {
            identity::set_settings(&state.pg, who, scope, patch).await?;
            Ok(None)
        }
        Cmd::IdentitySetSharePresence { on } => {
            identity::set_share_presence(&state.pg, who, on).await?;
            // Keep the emit-time cache on this connection in step (§4.2).
            handle.share_presence.store(on, Ordering::Relaxed);
            Ok(None)
        }

        Cmd::ChannelsSend {
            channel_id,
            client_uuid,
            body,
        } => Ok(Some(
            channels::send(state, who, channel_id, client_uuid, &body).await?,
        )),
        Cmd::ChannelsOpenDirect { number } => {
            Ok(Some(channels::open_direct(state, who, &number).await?))
        }
        Cmd::ChannelsCreate { name, members } => {
            Ok(Some(channels::create(state, who, name, members).await?))
        }
        Cmd::ChannelsList => {
            let list = channels::list(state, who).await?;
            Ok(Some(
                serde_json::to_value(list).map_err(anyhow::Error::from)?,
            ))
        }
        Cmd::ChannelsMarkDelivered {
            channel_id,
            up_to_seq,
        } => {
            channels::mark(state, who, channel_id, ReceiptKind::Delivered, up_to_seq).await?;
            Ok(None)
        }
        Cmd::ChannelsMarkRead {
            channel_id,
            up_to_seq,
        } => {
            channels::mark(state, who, channel_id, ReceiptKind::Read, up_to_seq).await?;
            Ok(None)
        }
        Cmd::ChannelsTyping { channel_id } => {
            channels::typing(state, who, channel_id).await?;
            Ok(None)
        }
        Cmd::ChannelsReact {
            channel_id,
            message_id,
            emoji,
        } => {
            channels::react(state, who, channel_id, message_id, &emoji, true).await?;
            Ok(None)
        }
        Cmd::ChannelsUnreact {
            channel_id,
            message_id,
            emoji,
        } => {
            channels::react(state, who, channel_id, message_id, &emoji, false).await?;
            Ok(None)
        }
        Cmd::ChannelsPin {
            channel_id,
            message_id,
        } => {
            channels::pin(state, who, channel_id, message_id, true).await?;
            Ok(None)
        }
        Cmd::ChannelsUnpin {
            channel_id,
            message_id,
        } => {
            channels::pin(state, who, channel_id, message_id, false).await?;
            Ok(None)
        }
        Cmd::ChannelsMemberAdd {
            channel_id,
            character_id,
        } => {
            channels::member_change(state, who, channel_id, character_id, true).await?;
            Ok(None)
        }
        Cmd::ChannelsMemberRemove {
            channel_id,
            character_id,
        } => {
            channels::member_change(state, who, channel_id, character_id, false).await?;
            Ok(None)
        }
        Cmd::ChannelsMembers { channel_id } => {
            let list = channels::members(state, who, channel_id).await?;
            Ok(Some(
                serde_json::to_value(list).map_err(anyhow::Error::from)?,
            ))
        }
        Cmd::ChannelsSetMuted { channel_id, muted } => {
            channels::set_muted(state, who, channel_id, muted).await?;
            Ok(None)
        }

        Cmd::MediaRequestUpload { kind, bytes, mime } => {
            let ticket = media::request_upload(state, who, kind, bytes, &mime).await?;
            Ok(Some(
                serde_json::to_value(ticket).map_err(anyhow::Error::from)?,
            ))
        }
        Cmd::MediaCommit { media_id } => {
            media::commit(state, who, media_id).await?;
            Ok(None)
        }

        Cmd::DirectoryContactUpsert {
            number,
            display_name,
            avatar_media,
            meta,
        } => {
            directory::contact_upsert(state, who, &number, &display_name, avatar_media, meta)
                .await?;
            Ok(None)
        }
        Cmd::DirectoryContactDelete { number } => {
            directory::contact_delete(state, who, &number).await?;
            Ok(None)
        }
        Cmd::DirectoryContacts => {
            let list = directory::contacts(state, who).await?;
            Ok(Some(
                serde_json::to_value(list).map_err(anyhow::Error::from)?,
            ))
        }
        Cmd::DirectoryBlock { number } => {
            directory::block(state, who, &number).await?;
            Ok(None)
        }
        Cmd::DirectoryUnblock { number } => {
            directory::unblock(state, who, &number).await?;
            Ok(None)
        }
        Cmd::DirectoryBlocks => {
            let nums = directory::blocks(state, who).await?;
            Ok(Some(
                serde_json::to_value(nums).map_err(anyhow::Error::from)?,
            ))
        }
        Cmd::DirectoryResolve { number } => {
            let res = directory::resolve_public(state, who, &number).await?;
            Ok(Some(
                serde_json::to_value(res).map_err(anyhow::Error::from)?,
            ))
        }
        Cmd::DirectoryListingCreate {
            app_id,
            kind,
            title,
            body,
            contact_number,
            ttl_secs,
        } => Ok(Some(
            directory::listing_create(
                state,
                who,
                &app_id,
                &kind,
                &title,
                body,
                &contact_number,
                ttl_secs,
            )
            .await?,
        )),
        Cmd::DirectoryListingDelete { id } => {
            directory::listing_delete(state, who, id).await?;
            Ok(None)
        }
        Cmd::DirectoryListings {
            app_id,
            cursor,
            limit,
        } => {
            let cur = cursor.as_deref().map(cursor::decode).transpose()?;
            let page = directory::listings(state, who, &app_id, cur, limit.unwrap_or(50)).await?;
            Ok(Some(json!({
                "items": page.items,
                "next_cursor": page.next_cursor,
            })))
        }

        Cmd::CallsStart {
            callee_number,
            video,
        } => Ok(Some(calls::start(state, who, &callee_number, video).await?)),
        Cmd::CallsAccept { call_id } => {
            calls::accept(state, who, call_id).await?;
            Ok(None)
        }
        Cmd::CallsDecline { call_id } => {
            calls::decline(state, who, call_id).await?;
            Ok(None)
        }
        Cmd::CallsHangup { call_id } => {
            calls::hangup(state, who, call_id).await?;
            Ok(None)
        }
        Cmd::CallsSignal {
            call_id,
            to,
            payload,
        } => {
            calls::signal(state, who, call_id, to, payload).await?;
            Ok(None)
        }

        Cmd::CallsGroupCreate {
            label,
            max_participants,
        } => Ok(Some(
            calls::group::create(state, who, label, max_participants).await?,
        )),
        Cmd::CallsGroupJoin { call_id } => {
            Ok(Some(calls::group::join(state, who, call_id).await?))
        }
        Cmd::CallsGroupLeave { call_id } => {
            calls::group::leave(state, who, call_id).await?;
            Ok(None)
        }
        Cmd::CallsGroupEnd { call_id } => {
            calls::group::end(state, who, call_id).await?;
            Ok(None)
        }

        Cmd::LedgerTransfer {
            from_account,
            to_account,
            amount,
            client_uuid,
        } => Ok(Some(
            ledger::transfer(state, who, from_account, to_account, amount, client_uuid).await?,
        )),
        Cmd::LedgerHold {
            account,
            amount,
            expires_in_secs,
        } => Ok(Some(
            ledger::hold(state, who, account, amount, expires_in_secs).await?,
        )),
        Cmd::LedgerCapture { hold_id, to } => {
            Ok(Some(ledger::capture(state, who, hold_id, to).await?))
        }
        Cmd::LedgerRelease { hold_id } => {
            ledger::release(state, who, hold_id).await?;
            Ok(None)
        }
        Cmd::LedgerWithdraw { amount } => Ok(Some(ledger::withdraw(state, who, amount).await?)),

        Cmd::FeedPost {
            app_id,
            body,
            media_ids,
        } => Ok(Some(
            feed::post(state, who, &app_id, &body, &media_ids).await?,
        )),
        Cmd::FeedDelete { app_id, post_id } => {
            feed::delete(state, who, &app_id, post_id).await?;
            Ok(None)
        }
        Cmd::FeedLike { app_id, post_id } => {
            feed::like(state, who, &app_id, post_id, true).await?;
            Ok(None)
        }
        Cmd::FeedUnlike { app_id, post_id } => {
            feed::like(state, who, &app_id, post_id, false).await?;
            Ok(None)
        }
        Cmd::FeedComment {
            app_id,
            post_id,
            body,
        } => Ok(Some(
            feed::comment(state, who, &app_id, post_id, &body).await?,
        )),
        Cmd::FeedFollow { app_id, account_id } => {
            feed::follow(state, who, &app_id, account_id, true).await?;
            Ok(None)
        }
        Cmd::FeedUnfollow { app_id, account_id } => {
            feed::follow(state, who, &app_id, account_id, false).await?;
            Ok(None)
        }

        Cmd::NotifySeen { ids } => {
            notify::seen(&state.pg, who, &ids).await?;
            Ok(None)
        }
        Cmd::NotifyClear => {
            notify::clear(&state.pg, who).await?;
            Ok(None)
        }
    }
}

/// Wire name for metrics/span labels — matches the serde tag.
fn wire_name(cmd: &Cmd) -> &'static str {
    match cmd {
        Cmd::Auth { .. } => "auth",
        Cmd::Sub { .. } => "sub",
        Cmd::Unsub { .. } => "unsub",
        Cmd::AuthRefresh => "auth.refresh",
        Cmd::IdentityMe => "identity.me",
        Cmd::IdentityAppLogin { .. } => "identity.app_login",
        Cmd::IdentityGetSettings { .. } => "identity.get_settings",
        Cmd::IdentitySetSettings { .. } => "identity.set_settings",
        Cmd::IdentitySetSharePresence { .. } => "identity.set_share_presence",
        Cmd::ChannelsSend { .. } => "channels.send",
        Cmd::ChannelsOpenDirect { .. } => "channels.open_direct",
        Cmd::ChannelsCreate { .. } => "channels.create",
        Cmd::ChannelsList => "channels.list",
        Cmd::ChannelsMarkDelivered { .. } => "channels.mark_delivered",
        Cmd::ChannelsMarkRead { .. } => "channels.mark_read",
        Cmd::ChannelsTyping { .. } => "channels.typing",
        Cmd::ChannelsReact { .. } => "channels.react",
        Cmd::ChannelsUnreact { .. } => "channels.unreact",
        Cmd::ChannelsPin { .. } => "channels.pin",
        Cmd::ChannelsUnpin { .. } => "channels.unpin",
        Cmd::ChannelsMemberAdd { .. } => "channels.member_add",
        Cmd::ChannelsMemberRemove { .. } => "channels.member_remove",
        Cmd::ChannelsMembers { .. } => "channels.members",
        Cmd::ChannelsSetMuted { .. } => "channels.set_muted",
        Cmd::MediaRequestUpload { .. } => "media.request_upload",
        Cmd::MediaCommit { .. } => "media.commit",
        Cmd::DirectoryContactUpsert { .. } => "directory.contact_upsert",
        Cmd::DirectoryContactDelete { .. } => "directory.contact_delete",
        Cmd::DirectoryContacts => "directory.contacts",
        Cmd::DirectoryBlock { .. } => "directory.block",
        Cmd::DirectoryUnblock { .. } => "directory.unblock",
        Cmd::DirectoryBlocks => "directory.blocks",
        Cmd::DirectoryResolve { .. } => "directory.resolve",
        Cmd::DirectoryListingCreate { .. } => "directory.listing_create",
        Cmd::DirectoryListingDelete { .. } => "directory.listing_delete",
        Cmd::DirectoryListings { .. } => "directory.listings",
        Cmd::CallsStart { .. } => "calls.start",
        Cmd::CallsAccept { .. } => "calls.accept",
        Cmd::CallsDecline { .. } => "calls.decline",
        Cmd::CallsHangup { .. } => "calls.hangup",
        Cmd::CallsSignal { .. } => "calls.signal",
        Cmd::CallsGroupCreate { .. } => "calls.group.create",
        Cmd::CallsGroupJoin { .. } => "calls.group.join",
        Cmd::CallsGroupLeave { .. } => "calls.group.leave",
        Cmd::CallsGroupEnd { .. } => "calls.group.end",
        Cmd::LedgerTransfer { .. } => "ledger.transfer",
        Cmd::LedgerHold { .. } => "ledger.hold",
        Cmd::LedgerCapture { .. } => "ledger.capture",
        Cmd::LedgerRelease { .. } => "ledger.release",
        Cmd::LedgerWithdraw { .. } => "ledger.withdraw",
        Cmd::FeedPost { .. } => "feed.post",
        Cmd::FeedDelete { .. } => "feed.delete",
        Cmd::FeedLike { .. } => "feed.like",
        Cmd::FeedUnlike { .. } => "feed.unlike",
        Cmd::FeedComment { .. } => "feed.comment",
        Cmd::FeedFollow { .. } => "feed.follow",
        Cmd::FeedUnfollow { .. } => "feed.unfollow",
        Cmd::NotifySeen { .. } => "notify.seen",
        Cmd::NotifyClear => "notify.clear",
    }
}
