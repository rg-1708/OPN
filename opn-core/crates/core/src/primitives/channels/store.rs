//! channels SQL (OPN-CORE.md §8, §10.2). Flat `pub async fn`s over the pool;
//! the handler layer in `mod.rs` does validation and post-commit fan-out.

use contracts::types::{ChannelSummary, MessagePreview};
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

    let callee = directory::resolve(&mut tx, number)
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
}

/// The caller's memberships (§10.2): channel row + own watermarks + a
/// last-message preview via a lateral join, one query, newest thread first.
pub async fn list_memberships(
    pool: &PgPool,
    world: Uuid,
    character: Uuid,
) -> Result<Vec<ChannelSummary>, Fail> {
    let mut tx = world_tx(pool, world).await?;
    let rows: Vec<SummaryRow> = sqlx::query_as(
        "SELECT c.id, c.kind, c.name, c.last_seq, \
                m.last_read_seq, m.last_delivered_seq, m.muted, \
                lm.seq AS lm_seq, lm.sender_character AS lm_sender, \
                lm.body AS lm_body, lm.created_at AS lm_created_at \
         FROM channel_members m \
         JOIN channels c ON c.id = m.channel_id \
         LEFT JOIN LATERAL ( \
             SELECT seq, sender_character, body, created_at FROM messages \
             WHERE channel_id = c.id ORDER BY seq DESC LIMIT 1 \
         ) lm ON true \
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
        })
        .collect())
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
