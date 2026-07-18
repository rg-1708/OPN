//! calls SQL (OPN-CORE.md §10.4). Flat async fns over the pool; `mod.rs` does
//! validation + post-commit fan-out, `fsm.rs` owns the transition logic. Every
//! query is world-scoped by `world_tx` (RLS). Lock order for transitions is the
//! session row first, then its participants (§10.4) — the one order, so two
//! concurrent transitions on the same call cannot deadlock.

use contracts::{CallKind, CallParticipant, CallParticipantState, CallSessionState, ErrCode};
use sqlx::PgPool;
use uuid::Uuid;

use super::fsm::{self, Action};
use crate::infra::db::world_tx;
use crate::infra::ids::new_id;
use crate::primitives::{directory, Fail};

// ── text ↔ enum (the DB stores lowercase text; contracts carries the enum) ──

fn kind_str(k: CallKind) -> &'static str {
    match k {
        CallKind::Voice => "voice",
        CallKind::Video => "video",
    }
}
fn parse_kind(s: &str) -> CallKind {
    match s {
        "video" => CallKind::Video,
        // Unknown/legacy rows read as voice — the neutral default.
        _ => CallKind::Voice,
    }
}
fn session_str(s: CallSessionState) -> &'static str {
    match s {
        CallSessionState::Ringing => "ringing",
        CallSessionState::Active => "active",
        CallSessionState::Ended => "ended",
    }
}
fn parse_session(s: &str) -> CallSessionState {
    match s {
        "active" => CallSessionState::Active,
        "ended" => CallSessionState::Ended,
        _ => CallSessionState::Ringing,
    }
}
fn part_str(p: CallParticipantState) -> &'static str {
    match p {
        CallParticipantState::Ringing => "ringing",
        CallParticipantState::Joined => "joined",
        CallParticipantState::Declined => "declined",
        CallParticipantState::Left => "left",
    }
}
fn parse_part(s: &str) -> CallParticipantState {
    match s {
        "joined" => CallParticipantState::Joined,
        "declined" => CallParticipantState::Declined,
        "left" => CallParticipantState::Left,
        _ => CallParticipantState::Ringing,
    }
}

/// Full session state for a `calls.state` snapshot (emit on every change and on
/// subscribe). Built by every store fn that mutates or reads a call.
pub struct CallSnapshot {
    pub call_id: Uuid,
    pub kind: CallKind,
    pub state: CallSessionState,
    pub participants: Vec<CallParticipant>,
}

fn to_participants(rows: Vec<(Uuid, String)>) -> Vec<CallParticipant> {
    rows.into_iter()
        .map(|(character_id, state)| CallParticipant {
            character_id,
            state: parse_part(&state),
        })
        .collect()
}

/// Result of `calls.start`: the new call plus what the handler needs to ring.
pub struct StartOutcome {
    pub call_id: Uuid,
    pub callee: Uuid,
    /// Caller's own number, for the ring's caller-ID (NULL if unassigned).
    pub caller_number: Option<String>,
}

/// `calls.start` (§10.4): resolve the number (blocked/unknown → `NotFound`,
/// privacy), reject self-call and a busy callee, then create a `ringing` session
/// with the caller `joined` and the callee `ringing`. One tx.
pub async fn start(
    pool: &PgPool,
    world: Uuid,
    caller: Uuid,
    caller_device: Uuid,
    number: &str,
    kind: CallKind,
) -> Result<StartOutcome, Fail> {
    let mut tx = world_tx(pool, world).await?;

    // Resolve through the directory seam: unknown OR blocked (either direction)
    // reads as None → NotFound, indistinguishable from no-such-number (§10.7).
    let callee = directory::resolve(&mut tx, caller, number)
        .await?
        .ok_or(Fail::Code(ErrCode::NotFound))?;
    if callee == caller {
        return Err(Fail::Code(ErrCode::Invalid));
    }

    // Busy = the callee already holds an active (ringing|joined) participant row
    // in a non-ended session. Uses the call_participants_active partial index.
    let busy: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM call_participants p JOIN call_sessions s ON s.id = p.call_id \
         WHERE p.character_id = $1 AND p.state IN ('ringing', 'joined') \
           AND s.state <> 'ended' LIMIT 1",
    )
    .bind(callee)
    .fetch_optional(&mut *tx)
    .await?;
    if busy.is_some() {
        // ponytail: a bare Conflict — the roadmap wants a "busy" detail on the
        // ack, but the wire error carries only a code. Add a payload detail if a
        // client needs to tell busy from other conflicts.
        return Err(Fail::Code(ErrCode::Conflict));
    }

    let call_id = new_id();
    sqlx::query(
        "INSERT INTO call_sessions (id, world_id, kind, state) VALUES ($1, $2, $3, 'ringing')",
    )
    .bind(call_id)
    .bind(world)
    .bind(kind_str(kind))
    .execute(&mut *tx)
    .await?;
    // Caller joins immediately (its own device); the callee rings (device set on
    // accept). One statement, two rows.
    sqlx::query(
        "INSERT INTO call_participants (call_id, world_id, character_id, device_id, state, joined_at) \
         VALUES ($1, $2, $3, $4, 'joined', now()), \
                ($1, $2, $5, NULL, 'ringing', NULL)",
    )
    .bind(call_id)
    .bind(world)
    .bind(caller)
    .bind(caller_device)
    .bind(callee)
    .execute(&mut *tx)
    .await?;

    let caller_number: Option<String> =
        sqlx::query_scalar("SELECT number FROM characters WHERE id = $1")
            .bind(caller)
            .fetch_one(&mut *tx)
            .await?;

    tx.commit().await?;
    Ok(StartOutcome {
        call_id,
        callee,
        caller_number,
    })
}

/// `calls.accept` / `decline` / `hangup` (§10.4): load session (`FOR UPDATE`)
/// then participants (`FOR UPDATE`, id-ordered so concurrent transitions on the
/// same call cannot deadlock), run the pure FSM, persist, and return the fresh
/// snapshot. `actor_device` is `Some` only for accept (records the joining
/// device). Missing call → `NotFound`; non-participant → `Forbidden`; illegal
/// transition → `Conflict`.
pub async fn transition(
    pool: &PgPool,
    world: Uuid,
    call_id: Uuid,
    actor: Uuid,
    actor_device: Option<Uuid>,
    action: Action,
) -> Result<CallSnapshot, Fail> {
    let mut tx = world_tx(pool, world).await?;

    let sess: Option<(String, String)> =
        sqlx::query_as("SELECT kind, state FROM call_sessions WHERE id = $1 FOR UPDATE")
            .bind(call_id)
            .fetch_optional(&mut *tx)
            .await?;
    let (kind, session_state) = sess.ok_or(Fail::Code(ErrCode::NotFound))?;
    let session = parse_session(&session_state);

    let parts: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT character_id, state FROM call_participants WHERE call_id = $1 \
         ORDER BY character_id FOR UPDATE",
    )
    .bind(call_id)
    .fetch_all(&mut *tx)
    .await?;

    let actor_state = parts
        .iter()
        .find(|(c, _)| *c == actor)
        .map(|(_, s)| parse_part(s))
        .ok_or(Fail::Code(ErrCode::Forbidden))?;
    let others: Vec<CallParticipantState> = parts
        .iter()
        .filter(|(c, _)| *c != actor)
        .map(|(_, s)| parse_part(s))
        .collect();

    let trans = fsm::apply(session, actor_state, &others, action)
        .map_err(|()| Fail::Code(ErrCode::Conflict))?;

    // Persist the actor's new state. device_id set only when provided (accept);
    // joined_at/left_at stamped once, on the first entry to that state.
    sqlx::query(
        "UPDATE call_participants \
         SET state = $3, \
             device_id = COALESCE($4, device_id), \
             joined_at = CASE WHEN $3 = 'joined' AND joined_at IS NULL THEN now() ELSE joined_at END, \
             left_at   = CASE WHEN $3 = 'left'   AND left_at   IS NULL THEN now() ELSE left_at   END \
         WHERE call_id = $1 AND character_id = $2",
    )
    .bind(call_id)
    .bind(actor)
    .bind(part_str(trans.participant))
    .bind(actor_device)
    .execute(&mut *tx)
    .await?;

    if trans.session != session {
        sqlx::query(
            "UPDATE call_sessions \
             SET state = $2, ended_at = CASE WHEN $2 = 'ended' THEN now() ELSE ended_at END \
             WHERE id = $1",
        )
        .bind(call_id)
        .bind(session_str(trans.session))
        .execute(&mut *tx)
        .await?;
    }

    // Re-read the (locked) participants for an accurate snapshot.
    let parts_now: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT character_id, state FROM call_participants WHERE call_id = $1 ORDER BY character_id",
    )
    .bind(call_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(CallSnapshot {
        call_id,
        kind: parse_kind(&kind),
        state: trans.session,
        participants: to_participants(parts_now),
    })
}

/// `sub call:<id>` authorization (§10.4, CDR-6): the caller must be a participant
/// (any state) or `Forbidden`. Missing/RLS-hidden call also → `Forbidden` (no
/// existence leak; call ids are unguessable v7 uuids anyway). Split from the
/// snapshot read so dispatch can register the subscription *before* reading state
/// (subscribe-first, like the `ch:` arm), so a live transition after registration
/// is delivered rather than lost.
pub async fn authorize_sub(
    pool: &PgPool,
    world: Uuid,
    call_id: Uuid,
    character: Uuid,
) -> Result<(), Fail> {
    let mut tx = world_tx(pool, world).await?;
    let is_part: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM call_participants WHERE call_id = $1 AND character_id = $2",
    )
    .bind(call_id)
    .bind(character)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    if is_part.is_none() {
        return Err(Fail::Code(ErrCode::Forbidden));
    }
    Ok(())
}

/// Full snapshot of a call for `calls.state` (snapshot-on-sub after
/// `authorize_sub`). A vanished call (reaped between authz and here — can't
/// normally happen since rows are never deleted) → `NotFound`.
pub async fn snapshot(pool: &PgPool, world: Uuid, call_id: Uuid) -> Result<CallSnapshot, Fail> {
    let mut tx = world_tx(pool, world).await?;
    let (kind, state): (String, String) =
        sqlx::query_as("SELECT kind, state FROM call_sessions WHERE id = $1")
            .bind(call_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Fail::Code(ErrCode::NotFound))?;
    let parts: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT character_id, state FROM call_participants WHERE call_id = $1 ORDER BY character_id",
    )
    .bind(call_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(CallSnapshot {
        call_id,
        kind: parse_kind(&kind),
        state: parse_session(&state),
        participants: to_participants(parts),
    })
}

/// `calls.signal` authorization (§10.4): both sender and `to` must be active
/// (ringing|joined) participants of a ringing/active session. Missing call →
/// `NotFound`; ended session → `Conflict`; either party inactive → `Forbidden`.
pub async fn authorize_signal(
    pool: &PgPool,
    world: Uuid,
    call_id: Uuid,
    sender: Uuid,
    to: Uuid,
) -> Result<(), Fail> {
    let mut tx = world_tx(pool, world).await?;
    let row: Option<(String, bool, bool)> = sqlx::query_as(
        "SELECT s.state, \
           EXISTS(SELECT 1 FROM call_participants \
                  WHERE call_id = $1 AND character_id = $2 AND state IN ('ringing', 'joined')), \
           EXISTS(SELECT 1 FROM call_participants \
                  WHERE call_id = $1 AND character_id = $3 AND state IN ('ringing', 'joined')) \
         FROM call_sessions s WHERE s.id = $1",
    )
    .bind(call_id)
    .bind(sender)
    .bind(to)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;

    let (state, sender_ok, to_ok) = row.ok_or(Fail::Code(ErrCode::NotFound))?;
    if parse_session(&state) == CallSessionState::Ended {
        return Err(Fail::Code(ErrCode::Conflict));
    }
    if !sender_ok || !to_ok {
        return Err(Fail::Code(ErrCode::Forbidden));
    }
    Ok(())
}

/// Janitor zombie-ring reap (§10.4): force-end any call still `ringing` past the
/// 60 s timeout — a ring nobody accepted, whether the callee never answered or
/// the caller crashed mid-ring (WS disconnect does NOT transition the caller's
/// participant row, so it stays `joined`; keying the reap on "no joined
/// participant" would therefore skip every real ring, since `start` always joins
/// the caller — see reflections 2026-07-18, Sprint 6 decision, and the adversarial
/// review that caught it). A ring only leaves `'ringing'` via `accept`, so this
/// reaps exactly the un-accepted ones and never an `active` call. Returns a
/// snapshot per ended session so the janitor emits a final `calls.state`.
/// Idempotent under the per-task advisory lock (rule 7); an already-non-ringing
/// session is excluded by the `state = 'ringing'` guard.
pub async fn reap_zombie_rings(pool: &PgPool, world: Uuid) -> anyhow::Result<Vec<CallSnapshot>> {
    let mut tx = world_tx(pool, world).await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('janitor:calls_reap'))")
        .execute(&mut *tx)
        .await?;
    let ended: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE call_sessions SET state = 'ended', ended_at = now() \
         WHERE state = 'ringing' AND created_at < now() - interval '60 seconds' \
         RETURNING id",
    )
    .fetch_all(&mut *tx)
    .await?;

    let mut snaps = Vec::with_capacity(ended.len());
    for id in ended {
        let (kind, state): (String, String) =
            sqlx::query_as("SELECT kind, state FROM call_sessions WHERE id = $1")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        let parts: Vec<(Uuid, String)> = sqlx::query_as(
            "SELECT character_id, state FROM call_participants WHERE call_id = $1 \
             ORDER BY character_id",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        snaps.push(CallSnapshot {
            call_id: id,
            kind: parse_kind(&kind),
            state: parse_session(&state),
            participants: to_participants(parts),
        });
    }
    tx.commit().await?;
    Ok(snaps)
}
