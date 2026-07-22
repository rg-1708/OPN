//! channels SQL (OPN-CORE.md §8, §10.2). Flat `pub async fn`s over the pool;
//! the handler layer in `mod.rs` does validation and post-commit fan-out.

use contracts::types::{
    ChannelMember, ChannelSummary, MessageItem, MessagePreview, ReactionItem, ReceiptKind,
};
use contracts::ErrCode;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::infra::db::world_tx;
use crate::infra::ids::new_id;
use crate::infra::timefmt::rfc3339;
use crate::primitives::{directory, Fail};

/// One member of a channel, captured in the send tx for post-commit routing.
pub struct Member {
    pub character_id: Uuid,
    pub muted: bool,
}

#[derive(sqlx::FromRow)]
struct MemberRow {
    character_id: Uuid,
    muted: bool,
}

/// Result of the send hot path. `deduped` = an idempotent retry hit an
/// existing row: the ack is the original and the caller fans out nothing.
pub struct SendOutcome {
    pub message_id: Uuid,
    pub seq: i64,
    pub created_at: OffsetDateTime,
    pub deduped: bool,
    /// All members (empty on a dedup hit — no fan-out needed).
    pub members: Vec<Member>,
}

/// The send hot path (§8), one transaction:
/// membership authz → per-channel `seq` under the channel row lock →
/// post-lock idempotency check → insert. The channel row lock is the
/// serialization point, so concurrent sends get a gapless `seq` and
/// concurrent identical `client_uuid`s dedupe against each other.
pub async fn send_message(
    pool: &PgPool,
    world: Uuid,
    sender: Uuid,
    channel_id: Uuid,
    client_uuid: Uuid,
    body: &serde_json::Value,
) -> Result<SendOutcome, Fail> {
    let mut tx = world_tx(pool, world).await?;

    // One membership read serves both authz and the post-commit notify list.
    // RLS scopes it to `world`, so a cross-world channel_id reads as empty →
    // Forbidden, same as a channel the sender simply isn't in (no existence
    // leak).
    let members: Vec<MemberRow> =
        sqlx::query_as("SELECT character_id, muted FROM channel_members WHERE channel_id = $1")
            .bind(channel_id)
            .fetch_all(&mut *tx)
            .await?;
    if !members.iter().any(|m| m.character_id == sender) {
        return Err(Fail::Code(ErrCode::Forbidden));
    }

    // Per-channel seq via the channel row lock (§8): concurrent senders
    // serialize here, which is what makes `seq` gapless and monotonic.
    let seq: i64 = sqlx::query_scalar(
        "UPDATE channels SET last_seq = last_seq + 1 WHERE id = $1 RETURNING last_seq",
    )
    .bind(channel_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(Fail::Code(ErrCode::NotFound))?;

    // Idempotency, checked AFTER taking the channel lock. This ordering is
    // load-bearing: two concurrent identical `client_uuid` sends serialize on
    // the channel row, so the loser sees the winner's committed row here and
    // dedupes. It also closes the cross-partition case the DB unique cannot —
    // the unique index carries `created_at` (the partition key), so it only
    // guards a same-month race; this query is keyed on (channel_id,
    // client_uuid) alone. DO NOT delete as "redundant": the index and this
    // check cover different races.
    let existing: Option<(Uuid, i64, OffsetDateTime)> = sqlx::query_as(
        "SELECT id, seq, created_at FROM messages WHERE channel_id = $1 AND client_uuid = $2",
    )
    .bind(channel_id)
    .bind(client_uuid)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((message_id, seq, created_at)) = existing {
        // Roll back the seq bump so a deduped retry leaves no gap.
        tx.rollback().await?;
        return Ok(SendOutcome {
            message_id,
            seq,
            created_at,
            deduped: true,
            members: Vec::new(),
        });
    }

    let message_id = new_id();
    let created_at: OffsetDateTime = sqlx::query_scalar(
        "INSERT INTO messages \
           (id, world_id, channel_id, seq, sender_character, body, client_uuid) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING created_at",
    )
    .bind(message_id)
    .bind(world)
    .bind(channel_id)
    .bind(seq)
    .bind(sender)
    .bind(body)
    .bind(client_uuid)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(SendOutcome {
        message_id,
        seq,
        created_at,
        deduped: false,
        members: members
            .into_iter()
            .map(|m| Member {
                character_id: m.character_id,
                muted: m.muted,
            })
            .collect(),
    })
}

/// Found-or-create the `dm` pair thread to `number` (§10.2). Resolves through
/// the directory seam; an unknown (or, from Sprint 5, blocked) number is
/// `NotFound`. Concurrent opens of the same pair converge on one channel via
/// the ordered-pair unique index.
pub async fn open_direct(
    pool: &PgPool,
    world: Uuid,
    caller: Uuid,
    number: &str,
) -> Result<Uuid, Fail> {
    let mut tx = world_tx(pool, world).await?;

    // Resolve through the directory seam: an unknown OR blocked number (either
    // direction) reads as None here, so a block is `NotFound` — indistinguishable
    // from no-such-number (§10.7 privacy).
    let callee = directory::resolve(&mut tx, caller, number)
        .await?
        .ok_or(Fail::Code(ErrCode::NotFound))?;
    if callee == caller {
        return Err(Fail::Code(ErrCode::Invalid));
    }

    // Ordered pair: sorted ids so (a,b) and (b,a) map to the same row.
    let (a, b) = if caller < callee {
        (caller, callee)
    } else {
        (callee, caller)
    };

    let created: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO channels (id, world_id, kind, pair_a, pair_b) \
         VALUES ($1, $2, 'dm', $3, $4) \
         ON CONFLICT (world_id, kind, pair_a, pair_b) WHERE pair_a IS NOT NULL \
         DO NOTHING RETURNING id",
    )
    .bind(new_id())
    .bind(world)
    .bind(a)
    .bind(b)
    .fetch_optional(&mut *tx)
    .await?;

    let channel_id = match created {
        Some(id) => {
            sqlx::query(
                "INSERT INTO channel_members (channel_id, world_id, character_id) \
                 VALUES ($1, $2, $3), ($1, $2, $4)",
            )
            .bind(id)
            .bind(world)
            .bind(a)
            .bind(b)
            .execute(&mut *tx)
            .await?;
            id
        }
        // Lost the create race (or the thread already existed): the winner's
        // row is committed and visible now.
        None => {
            sqlx::query_scalar(
                "SELECT id FROM channels \
             WHERE world_id = $1 AND kind = 'dm' AND pair_a = $2 AND pair_b = $3",
            )
            .bind(world)
            .bind(a)
            .bind(b)
            .fetch_one(&mut *tx)
            .await?
        }
    };

    tx.commit().await?;
    Ok(channel_id)
}

/// Create a group channel: creator + `members` (already deduped, self
/// removed, ≤ 32 by the handler). Every member must be a character of this
/// world (RLS-scoped count check) or the whole create is `Invalid`.
pub async fn create_group(
    pool: &PgPool,
    world: Uuid,
    creator: Uuid,
    name: Option<&str>,
    members: &[Uuid],
) -> Result<Uuid, Fail> {
    let mut tx = world_tx(pool, world).await?;

    if !members.is_empty() {
        let found: i64 = sqlx::query_scalar("SELECT count(*) FROM characters WHERE id = ANY($1)")
            .bind(members)
            .fetch_one(&mut *tx)
            .await?;
        if found as usize != members.len() {
            // A member id is unknown or belongs to another world (RLS hides it).
            return Err(Fail::Code(ErrCode::Invalid));
        }
    }

    let channel_id = new_id();
    sqlx::query("INSERT INTO channels (id, world_id, kind, name) VALUES ($1, $2, 'group', $3)")
        .bind(channel_id)
        .bind(world)
        .bind(name)
        .execute(&mut *tx)
        .await?;

    // Creator first, then the (deduped, self-free) member list, in one insert.
    let mut all: Vec<Uuid> = Vec::with_capacity(members.len() + 1);
    all.push(creator);
    all.extend_from_slice(members);
    sqlx::query(
        "INSERT INTO channel_members (channel_id, world_id, character_id) \
         SELECT $1, $2, u FROM unnest($3::uuid[]) AS u",
    )
    .bind(channel_id)
    .bind(world)
    .bind(&all)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(channel_id)
}

#[derive(sqlx::FromRow)]
struct SummaryRow {
    id: Uuid,
    kind: String,
    name: Option<String>,
    last_seq: i64,
    last_read_seq: i64,
    last_delivered_seq: i64,
    muted: bool,
    lm_seq: Option<i64>,
    lm_sender: Option<Uuid>,
    lm_body: Option<serde_json::Value>,
    lm_created_at: Option<OffsetDateTime>,
    peer_last_seen: Option<OffsetDateTime>,
    peer_number: Option<String>,
    peer_char: Option<Uuid>,
    peer_spoke: Option<bool>,
    server_id: Option<Uuid>,
    category: Option<String>,
    position: i32,
}

/// The caller's memberships (§10.2): channel row + own watermarks + a
/// last-message preview via a lateral join, one query, newest thread first.
/// For `dm` threads the second lateral pulls the other party's last-seen,
/// already gated on their `share_presence` (roadmap Sprint 4 item 10) — so a
/// non-sharing peer reads as `NULL`, indistinguishable from never-seen.
pub async fn list_memberships(
    pool: &PgPool,
    world: Uuid,
    character: Uuid,
) -> Result<Vec<ChannelSummary>, Fail> {
    let mut tx = world_tx(pool, world).await?;
    // The dm peer lateral carries the peer's presence-gated last-seen, their
    // number (gap #10), their character id, and whether they've spoken. The
    // `peer_spoke` EXISTS gates the character id (gap #4): it's revealed only
    // once the peer has emitted a message, so opening a DM to a number can't
    // resolve that number to a character before the peer ever interacts (§10.7).
    // ponytail: one EXISTS-on-messages per dm row; the list is bounded by the
    // caller's channel count, so no index beyond messages_channel_seq is needed.
    let rows: Vec<SummaryRow> = sqlx::query_as(
        "SELECT c.id, c.kind, c.name, c.last_seq, \
                c.server_id, c.category, c.position, \
                m.last_read_seq, m.last_delivered_seq, m.muted, \
                lm.seq AS lm_seq, lm.sender_character AS lm_sender, \
                lm.body AS lm_body, lm.created_at AS lm_created_at, \
                peer.last_seen AS peer_last_seen, \
                peer.peer_number AS peer_number, \
                peer.peer_char AS peer_char, \
                peer.peer_spoke AS peer_spoke \
         FROM channel_members m \
         JOIN channels c ON c.id = m.channel_id \
         LEFT JOIN LATERAL ( \
             SELECT seq, sender_character, body, created_at FROM messages \
             WHERE channel_id = c.id ORDER BY seq DESC LIMIT 1 \
         ) lm ON true \
         LEFT JOIN LATERAL ( \
             SELECT CASE WHEN pc.share_presence THEN pc.last_seen_at END AS last_seen, \
                    pc.number AS peer_number, \
                    pc.id AS peer_char, \
                    EXISTS(SELECT 1 FROM messages pmsg \
                           WHERE pmsg.channel_id = c.id AND pmsg.sender_character = pc.id) AS peer_spoke \
             FROM channel_members pm \
             JOIN characters pc ON pc.id = pm.character_id \
             WHERE c.kind = 'dm' AND pm.channel_id = c.id AND pm.character_id <> $1 \
             LIMIT 1 \
         ) peer ON true \
         WHERE m.character_id = $1 \
         ORDER BY COALESCE(lm.created_at, c.created_at) DESC",
    )
    .bind(character)
    .fetch_all(&mut *tx)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| ChannelSummary {
            channel_id: r.id,
            kind: r.kind,
            name: r.name,
            last_seq: r.last_seq,
            last_read_seq: r.last_read_seq,
            last_delivered_seq: r.last_delivered_seq,
            muted: r.muted,
            last_message: match (r.lm_seq, r.lm_sender, r.lm_body, r.lm_created_at) {
                (Some(seq), Some(sender), Some(body), Some(at)) => Some(MessagePreview {
                    seq,
                    sender,
                    body,
                    at: rfc3339(at),
                }),
                _ => None,
            },
            last_seen_at: r.peer_last_seen.map(rfc3339),
            // Only surface the peer's character id once they've spoken (gap #4).
            peer_character_id: match (r.peer_spoke, r.peer_char) {
                (Some(true), Some(id)) => Some(id),
                _ => None,
            },
            peer_number: r.peer_number,
            server_id: r.server_id,
            category: r.category,
            position: r.position,
        })
        .collect())
}

/// `channels.members` (§10.2, gap #3): the roster for a channel the caller is in.
/// Membership-gated (`Forbidden` for a non-member / RLS-hidden channel); returns
/// character ids + join times only — never phone numbers (§10.7 boundary).
pub async fn channel_members(
    pool: &PgPool,
    world: Uuid,
    channel_id: Uuid,
    caller: Uuid,
) -> Result<Vec<ChannelMember>, Fail> {
    let mut tx = world_tx(pool, world).await?;
    let member: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM channel_members WHERE channel_id = $1 AND character_id = $2",
    )
    .bind(channel_id)
    .bind(caller)
    .fetch_optional(&mut *tx)
    .await?;
    if member.is_none() {
        return Err(Fail::Code(ErrCode::Forbidden));
    }
    let rows: Vec<(Uuid, OffsetDateTime)> = sqlx::query_as(
        "SELECT character_id, joined_at FROM channel_members \
         WHERE channel_id = $1 ORDER BY joined_at, character_id",
    )
    .bind(channel_id)
    .fetch_all(&mut *tx)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(character_id, joined_at)| ChannelMember {
            character_id,
            joined_at: rfc3339(joined_at),
        })
        .collect())
}

/// `channels.set_muted` (§10.2, gap #3): set the caller's own mute flag on a
/// channel. Idempotent (re-setting the same value still matches the row);
/// `Forbidden` when the caller isn't a member (no row updated).
pub async fn set_muted(
    pool: &PgPool,
    world: Uuid,
    channel_id: Uuid,
    caller: Uuid,
    muted: bool,
) -> Result<(), Fail> {
    let mut tx = world_tx(pool, world).await?;
    let updated: Option<i32> = sqlx::query_scalar(
        "UPDATE channel_members SET muted = $3 \
         WHERE channel_id = $1 AND character_id = $2 RETURNING 1",
    )
    .bind(channel_id)
    .bind(caller)
    .bind(muted)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    if updated.is_some() {
        Ok(())
    } else {
        Err(Fail::Code(ErrCode::Forbidden))
    }
}

/// `sub ch:<id>` authorization (§4.4): membership only. Non-member (and
/// unknown-channel, RLS-hidden) → `Forbidden`, no existence leak.
pub async fn is_member(
    pool: &PgPool,
    world: Uuid,
    character: Uuid,
    channel_id: Uuid,
) -> Result<bool, Fail> {
    let mut tx = world_tx(pool, world).await?;
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM channel_members WHERE channel_id = $1 AND character_id = $2",
    )
    .bind(channel_id)
    .bind(character)
    .fetch_optional(&mut *tx)
    .await?;
    Ok(found.is_some())
}

// sqlx 0.9 wants `'static` SQL (no `&format!`), so the two watermark columns
// are two literal statements picked by `match` rather than an interpolated
// column name (reflections 2026-07-18, Sprint 1 decision 7).
const MARK_DELIVERED_SQL: &str =
    "UPDATE channel_members SET last_delivered_seq = LEAST($3, (SELECT last_seq FROM channels WHERE id = $1)) \
     WHERE channel_id = $1 AND character_id = $2 \
       AND last_delivered_seq < LEAST($3, (SELECT last_seq FROM channels WHERE id = $1)) \
     RETURNING last_delivered_seq";
const MARK_READ_SQL: &str =
    "UPDATE channel_members SET last_read_seq = LEAST($3, (SELECT last_seq FROM channels WHERE id = $1)) \
     WHERE channel_id = $1 AND character_id = $2 \
       AND last_read_seq < LEAST($3, (SELECT last_seq FROM channels WHERE id = $1)) \
     RETURNING last_read_seq";

/// Advance a watermark monotonically (§10.2), clamped to the channel's
/// `last_seq` so a client can never mark past what exists. Returns:
/// - `Some(seq)` — the watermark moved to `seq`; the caller emits a receipt.
/// - `None` — the caller is a member but the watermark did not move (a regress
///   or repeat); idempotent no-op, no event.
/// - `Err(Forbidden)` — the caller is not a member (or the channel is
///   RLS-hidden), indistinguishable from a foreign channel.
pub async fn mark_watermark(
    pool: &PgPool,
    world: Uuid,
    channel_id: Uuid,
    character: Uuid,
    kind: ReceiptKind,
    up_to_seq: i64,
) -> Result<Option<i64>, Fail> {
    let sql = match kind {
        ReceiptKind::Delivered => MARK_DELIVERED_SQL,
        ReceiptKind::Read => MARK_READ_SQL,
    };
    let mut tx = world_tx(pool, world).await?;
    let moved: Option<i64> = sqlx::query_scalar(sql)
        .bind(channel_id)
        .bind(character)
        .bind(up_to_seq)
        .fetch_optional(&mut *tx)
        .await?;
    if moved.is_some() {
        tx.commit().await?;
        return Ok(moved);
    }
    // No move: distinguish a member no-op from a non-member. One indexed read.
    let member: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM channel_members WHERE channel_id = $1 AND character_id = $2",
    )
    .bind(channel_id)
    .bind(character)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    if member.is_some() {
        Ok(None)
    } else {
        Err(Fail::Code(ErrCode::Forbidden))
    }
}

/// Membership + "this message is in this channel" in one round trip. Returns
/// `Forbidden` for a non-member, `NotFound` for a message not in the channel
/// (or RLS-hidden). Shared by react and pin.
async fn member_and_message(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    channel_id: Uuid,
    character: Uuid,
    message_id: Uuid,
) -> Result<(), Fail> {
    let row: (bool, bool) = sqlx::query_as(
        "SELECT \
           EXISTS(SELECT 1 FROM channel_members WHERE channel_id = $1 AND character_id = $2), \
           EXISTS(SELECT 1 FROM messages WHERE id = $3 AND channel_id = $1)",
    )
    .bind(channel_id)
    .bind(character)
    .bind(message_id)
    .fetch_one(&mut **tx)
    .await?;
    if !row.0 {
        return Err(Fail::Code(ErrCode::Forbidden));
    }
    if !row.1 {
        return Err(Fail::Code(ErrCode::NotFound));
    }
    Ok(())
}

/// Add or remove a reaction (§10.2), keyed `(message_id, character, emoji)`.
/// Returns `true` if the set actually changed (so the caller emits exactly one
/// event); a repeat add / absent remove is a `false` no-op.
pub async fn react(
    pool: &PgPool,
    world: Uuid,
    channel_id: Uuid,
    character: Uuid,
    message_id: Uuid,
    emoji: &str,
    add: bool,
) -> Result<bool, Fail> {
    let mut tx = world_tx(pool, world).await?;
    member_and_message(&mut tx, channel_id, character, message_id).await?;
    let changed = if add {
        sqlx::query(
            "INSERT INTO reactions (world_id, channel_id, message_id, character_id, emoji) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
        )
        .bind(world)
        .bind(channel_id)
        .bind(message_id)
        .bind(character)
        .bind(emoji)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0
    } else {
        sqlx::query(
            "DELETE FROM reactions \
             WHERE message_id = $1 AND character_id = $2 AND emoji = $3",
        )
        .bind(message_id)
        .bind(character)
        .bind(emoji)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0
    };
    tx.commit().await?;
    Ok(changed)
}

/// Max pinned messages per channel (§10.2). At the cap a new pin is `Conflict`.
const PINS_MAX: i64 = 50;

/// Pin or unpin a message (§10.2). The channel row is locked `FOR UPDATE`
/// first: it is already the send serialization point, so counting under it
/// closes the count-then-insert race at the 50 cap without a table lock.
/// Returns whether the pin set changed (one event per real change).
pub async fn pin(
    pool: &PgPool,
    world: Uuid,
    channel_id: Uuid,
    character: Uuid,
    message_id: Uuid,
    add: bool,
) -> Result<bool, Fail> {
    let mut tx = world_tx(pool, world).await?;
    // Lock the channel (existence + serialization). RLS-hidden → NotFound.
    let locked: Option<i32> = sqlx::query_scalar("SELECT 1 FROM channels WHERE id = $1 FOR UPDATE")
        .bind(channel_id)
        .fetch_optional(&mut *tx)
        .await?;
    if locked.is_none() {
        return Err(Fail::Code(ErrCode::NotFound));
    }

    let changed = if add {
        member_and_message(&mut tx, channel_id, character, message_id).await?;
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM channel_pins WHERE channel_id = $1")
                .bind(channel_id)
                .fetch_one(&mut *tx)
                .await?;
        if count >= PINS_MAX {
            return Err(Fail::Code(ErrCode::Conflict));
        }
        sqlx::query(
            "INSERT INTO channel_pins (channel_id, world_id, message_id, pinned_by) \
             VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
        )
        .bind(channel_id)
        .bind(world)
        .bind(message_id)
        .bind(character)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0
    } else {
        // Unpin only needs membership, not message-in-channel (the pin row
        // implies it) — but a non-member must still not touch pins.
        let member: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM channel_members WHERE channel_id = $1 AND character_id = $2",
        )
        .bind(channel_id)
        .bind(character)
        .fetch_optional(&mut *tx)
        .await?;
        if member.is_none() {
            return Err(Fail::Code(ErrCode::Forbidden));
        }
        sqlx::query("DELETE FROM channel_pins WHERE channel_id = $1 AND message_id = $2")
            .bind(channel_id)
            .bind(message_id)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            > 0
    };
    tx.commit().await?;
    Ok(changed)
}

/// Add or remove a group member (§10.2). Group kind only (`Conflict` on
/// dm/sms); the actor must be a member (`Forbidden`); the target must be a
/// character of this world (`Invalid`). Returns whether membership changed.
pub async fn member_change(
    pool: &PgPool,
    world: Uuid,
    channel_id: Uuid,
    actor: Uuid,
    target: Uuid,
    add: bool,
) -> Result<bool, Fail> {
    let mut tx = world_tx(pool, world).await?;

    // Lock + kind check: RLS-hidden/unknown → NotFound; non-group → Conflict.
    // Server channels are Conflict too — their membership mirrors
    // server_members and is only changed via servers.member_* (§10.2a).
    let row: Option<(String, Option<Uuid>)> =
        sqlx::query_as("SELECT kind, server_id FROM channels WHERE id = $1 FOR UPDATE")
            .bind(channel_id)
            .fetch_optional(&mut *tx)
            .await?;
    match row {
        None => return Err(Fail::Code(ErrCode::NotFound)),
        Some((_, Some(_))) => return Err(Fail::Code(ErrCode::Conflict)),
        Some((k, None)) if k == "group" => {}
        Some(_) => return Err(Fail::Code(ErrCode::Conflict)),
    }

    // Actor must be a member.
    let actor_member: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM channel_members WHERE channel_id = $1 AND character_id = $2",
    )
    .bind(channel_id)
    .bind(actor)
    .fetch_optional(&mut *tx)
    .await?;
    if actor_member.is_none() {
        return Err(Fail::Code(ErrCode::Forbidden));
    }

    let changed = if add {
        // Target must be a real character of this world (RLS-scoped).
        let exists: Option<i32> = sqlx::query_scalar("SELECT 1 FROM characters WHERE id = $1")
            .bind(target)
            .fetch_optional(&mut *tx)
            .await?;
        if exists.is_none() {
            return Err(Fail::Code(ErrCode::Invalid));
        }
        sqlx::query(
            "INSERT INTO channel_members (channel_id, world_id, character_id) \
             VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(channel_id)
        .bind(world)
        .bind(target)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0
    } else {
        sqlx::query("DELETE FROM channel_members WHERE channel_id = $1 AND character_id = $2")
            .bind(channel_id)
            .bind(target)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            > 0
    };
    tx.commit().await?;
    Ok(changed)
}

#[derive(sqlx::FromRow)]
struct MsgRow {
    id: Uuid,
    seq: i64,
    sender_character: Uuid,
    body: serde_json::Value,
    created_at: OffsetDateTime,
}

impl From<MsgRow> for MessageItem {
    fn from(r: MsgRow) -> MessageItem {
        MessageItem {
            message_id: r.id,
            seq: r.seq,
            sender: r.sender_character,
            body: r.body,
            at: rfc3339(r.created_at),
            // Resume replay re-pushes messages as live `channels.message` events,
            // which carry no reaction/pin state (the client heals those from the
            // live reaction/pin events); only the HTTP cold-load (`history`)
            // populates them. Defaults keep this path unchanged.
            pinned: false,
            reactions: Vec::new(),
        }
    }
}

/// A history row enriched with the durable reaction/pin state a cold-load must
/// carry (gap #2): `pinned` and the message's reactions aggregated as JSON.
#[derive(sqlx::FromRow)]
struct HistRow {
    id: Uuid,
    seq: i64,
    sender_character: Uuid,
    body: serde_json::Value,
    created_at: OffsetDateTime,
    pinned: bool,
    reactions: serde_json::Value,
}

/// Resume replay (§4.4): messages after `after_seq`, ascending, capped. The
/// sub authorization already ran in dispatch, so no membership check here. The
/// caller pushes these as `channels.message` events before the sub ack and, if
/// the cap is hit exactly, a `channels.resume_overflow`.
pub async fn replay_since(
    pool: &PgPool,
    world: Uuid,
    channel_id: Uuid,
    after_seq: i64,
    limit: i64,
) -> Result<Vec<MessageItem>, Fail> {
    let mut tx = world_tx(pool, world).await?;
    let rows: Vec<MsgRow> = sqlx::query_as(
        "SELECT id, seq, sender_character, body, created_at FROM messages \
         WHERE channel_id = $1 AND seq > $2 ORDER BY seq ASC LIMIT $3",
    )
    .bind(channel_id)
    .bind(after_seq)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await?;
    Ok(rows.into_iter().map(MessageItem::from).collect())
}

/// History page (§6): messages descending by seq, keyset on `before_seq`.
/// Membership required (`Forbidden` otherwise). Seq-keyed, not the time cursor.
pub async fn history(
    pool: &PgPool,
    world: Uuid,
    channel_id: Uuid,
    character: Uuid,
    before_seq: Option<i64>,
    limit: i64,
) -> Result<Vec<MessageItem>, Fail> {
    let mut tx = world_tx(pool, world).await?;
    let member: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM channel_members WHERE channel_id = $1 AND character_id = $2",
    )
    .bind(channel_id)
    .bind(character)
    .fetch_optional(&mut *tx)
    .await?;
    if member.is_none() {
        return Err(Fail::Code(ErrCode::Forbidden));
    }
    // Each row carries its pinned flag and reactions so a reload rebuilds the
    // full message state (gap #2). Both are correlated subqueries per row — the
    // page is capped at 100, so this is bounded work; `reactions_by_message`
    // and the `channel_pins` PK index both serve these lookups.
    let rows: Vec<HistRow> = sqlx::query_as(
        "SELECT m.id, m.seq, m.sender_character, m.body, m.created_at, \
                EXISTS(SELECT 1 FROM channel_pins cp \
                       WHERE cp.channel_id = $1 AND cp.message_id = m.id) AS pinned, \
                COALESCE((SELECT json_agg(json_build_object( \
                            'emoji', r.emoji, 'character_id', r.character_id) \
                            ORDER BY r.created_at) \
                          FROM reactions r WHERE r.message_id = m.id), '[]'::json) AS reactions \
         FROM messages m \
         WHERE m.channel_id = $1 AND ($2::bigint IS NULL OR m.seq < $2) \
         ORDER BY m.seq DESC LIMIT $3",
    )
    .bind(channel_id)
    .bind(before_seq)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let reactions: Vec<ReactionItem> =
            serde_json::from_value(r.reactions).map_err(|e| Fail::Internal(e.into()))?;
        out.push(MessageItem {
            message_id: r.id,
            seq: r.seq,
            sender: r.sender_character,
            body: r.body,
            at: rfc3339(r.created_at),
            pinned: r.pinned,
            reactions,
        });
    }
    Ok(out)
}
