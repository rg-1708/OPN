//! calls SQL (OPN-CORE.md §10.4). Flat async fns over the pool; `mod.rs` does
//! validation + post-commit fan-out, `fsm.rs` owns the transition logic. Every
//! query is world-scoped by `world_tx` (RLS). Lock order for transitions is the
//! session row first, then its participants (§10.4) — the one order, so two
//! concurrent transitions on the same call cannot deadlock.

use contracts::{
    CallKind, CallParticipant, CallParticipantState, CallSessionState, ErrCode, Topology,
};
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
fn parse_topology(s: &str) -> Topology {
    match s {
        "sfu" => Topology::Sfu,
        // Unknown/legacy rows read as p2p — the neutral default (matches the
        // column default and the `Topology` derive).
        _ => Topology::P2p,
    }
}

/// Full session state for a `calls.state` snapshot (emit on every change and on
/// subscribe). Built by every store fn that mutates or reads a call.
pub struct CallSnapshot {
    pub call_id: Uuid,
    pub kind: CallKind,
    pub state: CallSessionState,
    pub participants: Vec<CallParticipant>,
    /// `p2p` for 1:1 calls, `sfu` for group calls — decides which snapshot event
    /// (`calls.state` vs `calls.group.state`) the emit path builds.
    pub topology: Topology,
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

    let sess: Option<(String, String, String)> =
        sqlx::query_as("SELECT kind, state, topology FROM call_sessions WHERE id = $1 FOR UPDATE")
            .bind(call_id)
            .fetch_optional(&mut *tx)
            .await?;
    let (kind, session_state, topology) = sess.ok_or(Fail::Code(ErrCode::NotFound))?;
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
        topology: parse_topology(&topology),
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
    let (kind, state, topology): (String, String, String) =
        sqlx::query_as("SELECT kind, state, topology FROM call_sessions WHERE id = $1")
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
        topology: parse_topology(&topology),
    })
}

/// `GET /v1/tenants/self/calls/active` (§5): every non-ended session in the
/// world with its participants, for the tenant link's re-sync on (re)connect.
/// N+1 over sessions, bounded by the handful of concurrent calls — no cursor,
/// per the design (§5). One `world_tx` so RLS scopes both reads.
pub async fn active_calls(pool: &PgPool, world: Uuid) -> Result<Vec<CallSnapshot>, Fail> {
    let mut tx = world_tx(pool, world).await?;
    let sessions: Vec<(Uuid, String, String, String)> = sqlx::query_as(
        "SELECT id, kind, state, topology FROM call_sessions WHERE state <> 'ended' \
         ORDER BY created_at",
    )
    .fetch_all(&mut *tx)
    .await?;
    let mut out = Vec::with_capacity(sessions.len());
    for (id, kind, state, topology) in sessions {
        let parts: Vec<(Uuid, String)> = sqlx::query_as(
            "SELECT character_id, state FROM call_participants WHERE call_id = $1 \
             ORDER BY character_id",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        out.push(CallSnapshot {
            call_id: id,
            kind: parse_kind(&kind),
            state: parse_session(&state),
            participants: to_participants(parts),
            topology: parse_topology(&topology),
        });
    }
    tx.commit().await?;
    Ok(out)
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

/// Janitor orphaned-active-call candidates (§10.4, §5): every `active` session
/// older than 60 s with its currently-`joined` character ids. The janitor then
/// asks the registry which are offline; a session with **no** joined participant
/// still online is a double-crash orphan (a WS disconnect never transitions the
/// row, §5) and gets ended by `end_active_orphans` so the link receives its
/// `clear`. Age-gated so a call mid-setup (both briefly between sockets) is
/// spared. Returns `(call_id, joined_character_ids)`.
pub async fn active_reap_candidates(
    pool: &PgPool,
    world: Uuid,
) -> anyhow::Result<Vec<(Uuid, Vec<Uuid>)>> {
    let mut tx = world_tx(pool, world).await?;
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM call_sessions \
         WHERE state = 'active' AND created_at < now() - interval '60 seconds'",
    )
    .fetch_all(&mut *tx)
    .await?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let joined: Vec<Uuid> = sqlx::query_scalar(
            "SELECT character_id FROM call_participants WHERE call_id = $1 AND state = 'joined'",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        out.push((id, joined));
    }
    tx.commit().await?;
    Ok(out)
}

/// Force-end the given orphaned `active` sessions (§10.4, §5), returning a
/// snapshot per newly-ended one so the janitor emits the final `calls.state` and
/// the link `clear`. The `AND state = 'active'` guard makes it idempotent and
/// safe against a concurrent `hangup` that already ended the call (rule 7). Under
/// the per-task advisory lock, like the ring reap.
pub async fn end_active_orphans(
    pool: &PgPool,
    world: Uuid,
    ids: &[Uuid],
) -> anyhow::Result<Vec<CallSnapshot>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut tx = world_tx(pool, world).await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('janitor:calls_reap_orphaned'))")
        .execute(&mut *tx)
        .await?;
    let ended: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE call_sessions SET state = 'ended', ended_at = now() \
         WHERE id = ANY($1) AND state = 'active' RETURNING id",
    )
    .bind(ids)
    .fetch_all(&mut *tx)
    .await?;

    let mut snaps = Vec::with_capacity(ended.len());
    for id in ended {
        let (kind, state, topology): (String, String, String) =
            sqlx::query_as("SELECT kind, state, topology FROM call_sessions WHERE id = $1")
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
            topology: parse_topology(&topology),
        });
    }
    tx.commit().await?;
    Ok(snaps)
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
        let (kind, state, topology): (String, String, String) =
            sqlx::query_as("SELECT kind, state, topology FROM call_sessions WHERE id = $1")
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
            topology: parse_topology(&topology),
        });
    }
    tx.commit().await?;
    Ok(snaps)
}

// ── group calls (opn-group-calls.md G1) ──────────────────────────────────────
//
// Group sessions reuse these tables with topology='sfu'. No ringing state — the
// join model puts a participant straight to 'joined'. Every mutation returns a
// `CallSnapshot` (topology=Sfu) so the caller emits `calls.group.state`. Media
// rides the LiveKit sidecar; Core owns only membership + the SFU room name.

/// Read a group session's full snapshot inside an open tx (state, participants,
/// topology). Every caller has already established the row exists under the
/// session lock, so a missing row here is an internal invariant break rather
/// than a user-facing `NotFound`. Returns `anyhow::Result`; `Fail` converts from
/// it for the `Result<_, Fail>` callers.
async fn group_snapshot_in_tx(
    tx: &mut sqlx::PgConnection,
    call_id: Uuid,
) -> anyhow::Result<CallSnapshot> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT state, topology FROM call_sessions WHERE id = $1 AND topology = 'sfu'")
            .bind(call_id)
            .fetch_optional(&mut *tx)
            .await?;
    let (state, topology) =
        row.ok_or_else(|| anyhow::anyhow!("group session {call_id} vanished under lock"))?;
    let parts: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT character_id, state FROM call_participants WHERE call_id = $1 ORDER BY character_id",
    )
    .bind(call_id)
    .fetch_all(&mut *tx)
    .await?;
    Ok(CallSnapshot {
        call_id,
        kind: CallKind::Voice,
        state: parse_session(&state),
        participants: to_participants(parts),
        topology: parse_topology(&topology),
    })
}

/// `calls.group.create` (G1): any tenant member creates an active SFU session
/// with `sfu_room_id = "grp_<id>"`, auto-joining as the first participant (the
/// creator, identified later as the earliest `joined_at`). Returns the initial
/// snapshot. `label`/`max_participants` are not persisted — there is no column
/// for them (0014 adds only topology + sfu_room_id); the participant cap is
/// enforced at join. Fails `Conflict` when the world is already at its
/// concurrent-room ceiling (`max_rooms`, G3).
pub async fn group_create(
    pool: &PgPool,
    world: Uuid,
    creator: Uuid,
    creator_device: Uuid,
    max_rooms: i64,
) -> Result<CallSnapshot, Fail> {
    let mut tx = world_tx(pool, world).await?;
    // Per-tenant concurrent-room ceiling (G3). RLS (app.world_id, set by
    // world_tx) scopes the count to this world, so no explicit world filter.
    // ponytail: soft cap — two racing creates can both pass and overshoot by
    // one room under READ COMMITTED; fine for an anti-abuse ceiling. Per-world
    // advisory lock only if a hard invariant is ever needed.
    let active_rooms: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM call_sessions WHERE topology = 'sfu' AND state = 'active'",
    )
    .fetch_one(&mut *tx)
    .await?;
    super::group::rooms_admit(active_rooms, max_rooms).map_err(Fail::Code)?;
    let call_id = new_id();
    let room = format!("grp_{call_id}");
    sqlx::query(
        "INSERT INTO call_sessions (id, world_id, kind, state, topology, sfu_room_id) \
         VALUES ($1, $2, 'voice', 'active', 'sfu', $3)",
    )
    .bind(call_id)
    .bind(world)
    .bind(&room)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO call_participants (call_id, world_id, character_id, device_id, state, joined_at) \
         VALUES ($1, $2, $3, $4, 'joined', now())",
    )
    .bind(call_id)
    .bind(world)
    .bind(creator)
    .bind(creator_device)
    .execute(&mut *tx)
    .await?;
    let snap = group_snapshot_in_tx(&mut tx, call_id).await?;
    tx.commit().await?;
    Ok(snap)
}

/// `calls.group.join` (G1): membership row → 'joined' (upsert; rejoin allowed).
/// Ended session or a full room (already `cap` *other* participants joined) →
/// `Conflict`. Returns the fresh snapshot plus the SFU room name for the token
/// mint. No ringing — the join model admits directly.
pub async fn group_join(
    pool: &PgPool,
    world: Uuid,
    call_id: Uuid,
    character: Uuid,
    device: Uuid,
    cap: i64,
) -> Result<(CallSnapshot, String), Fail> {
    let mut tx = world_tx(pool, world).await?;
    let row: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT state, sfu_room_id FROM call_sessions WHERE id = $1 AND topology = 'sfu' FOR UPDATE",
    )
    .bind(call_id)
    .fetch_optional(&mut *tx)
    .await?;
    let (state, room) = row.ok_or(Fail::Code(ErrCode::NotFound))?;
    // Room full = `cap` OTHER characters already joined (exclude self so a rejoin
    // is never rejected by its own seat). Admission rule is the pure
    // `group::join_admits` — ended or full → `Conflict` (ErrCode is closed).
    let others_joined: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM call_participants \
         WHERE call_id = $1 AND state = 'joined' AND character_id <> $2",
    )
    .bind(call_id)
    .bind(character)
    .fetch_one(&mut *tx)
    .await?;
    super::group::join_admits(parse_session(&state), others_joined, cap).map_err(Fail::Code)?;
    // joined_at stamped once (COALESCE) so the earliest joiner (the creator) is
    // stable across rejoins — the `group_end` privilege check keys on it.
    sqlx::query(
        "INSERT INTO call_participants (call_id, world_id, character_id, device_id, state, joined_at) \
         VALUES ($1, $2, $3, $4, 'joined', now()) \
         ON CONFLICT (call_id, character_id) DO UPDATE \
           SET state = 'joined', device_id = $4, \
               joined_at = COALESCE(call_participants.joined_at, now())",
    )
    .bind(call_id)
    .bind(world)
    .bind(character)
    .bind(device)
    .execute(&mut *tx)
    .await?;
    let snap = group_snapshot_in_tx(&mut tx, call_id).await?;
    tx.commit().await?;
    let room = room.unwrap_or_else(|| format!("grp_{call_id}"));
    Ok((snap, room))
}

/// `calls.group.leave` (G1): participant → 'left'. Non-participant → `Forbidden`.
/// The last joined leaving ends the session (empty room). Idempotent.
pub async fn group_leave(
    pool: &PgPool,
    world: Uuid,
    call_id: Uuid,
    character: Uuid,
) -> Result<CallSnapshot, Fail> {
    let mut tx = world_tx(pool, world).await?;
    let state: Option<String> =
        sqlx::query_scalar("SELECT state FROM call_sessions WHERE id = $1 AND topology = 'sfu' FOR UPDATE")
            .bind(call_id)
            .fetch_optional(&mut *tx)
            .await?;
    state.ok_or(Fail::Code(ErrCode::NotFound))?;
    let updated = sqlx::query(
        "UPDATE call_participants SET state = 'left', left_at = COALESCE(left_at, now()) \
         WHERE call_id = $1 AND character_id = $2",
    )
    .bind(call_id)
    .bind(character)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(Fail::Code(ErrCode::Forbidden));
    }
    end_if_empty(&mut tx, call_id).await?;
    let snap = group_snapshot_in_tx(&mut tx, call_id).await?;
    tx.commit().await?;
    Ok(snap)
}

/// `calls.group.end` (G1): creator-only teardown → session 'ended'. The creator
/// is the earliest joiner (min `joined_at`); anyone else → `Forbidden`.
pub async fn group_end(
    pool: &PgPool,
    world: Uuid,
    call_id: Uuid,
    character: Uuid,
) -> Result<CallSnapshot, Fail> {
    let mut tx = world_tx(pool, world).await?;
    let state: Option<String> =
        sqlx::query_scalar("SELECT state FROM call_sessions WHERE id = $1 AND topology = 'sfu' FOR UPDATE")
            .bind(call_id)
            .fetch_optional(&mut *tx)
            .await?;
    state.ok_or(Fail::Code(ErrCode::NotFound))?;
    // ponytail: creator = earliest joiner. No creator column (0014 adds only two
    // columns); add a `role`/`created_by` column if co-hosts ever need to end.
    let creator: Option<Uuid> = sqlx::query_scalar(
        "SELECT character_id FROM call_participants WHERE call_id = $1 AND joined_at IS NOT NULL \
         ORDER BY joined_at, character_id LIMIT 1",
    )
    .bind(call_id)
    .fetch_optional(&mut *tx)
    .await?;
    if creator != Some(character) {
        return Err(Fail::Code(ErrCode::Forbidden));
    }
    sqlx::query(
        "UPDATE call_sessions SET state = 'ended', ended_at = now() \
         WHERE id = $1 AND state <> 'ended'",
    )
    .bind(call_id)
    .execute(&mut *tx)
    .await?;
    let snap = group_snapshot_in_tx(&mut tx, call_id).await?;
    tx.commit().await?;
    Ok(snap)
}

/// End an SFU session if no participant is still 'joined' (empty-room rule). The
/// `state <> 'ended'` guard makes it idempotent. Called by leave and the
/// participant_left webhook.
async fn end_if_empty(tx: &mut sqlx::PgConnection, call_id: Uuid) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE call_sessions SET state = 'ended', ended_at = now() \
         WHERE id = $1 AND state <> 'ended' \
           AND NOT EXISTS (SELECT 1 FROM call_participants p \
                           WHERE p.call_id = $1 AND p.state = 'joined')",
    )
    .bind(call_id)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// Janitor empty-group-room reap (G1): active SFU sessions older than
/// `reap_secs` with zero joined participants are force-ended (a room whose last
/// participant vanished without a clean leave/webhook). Returns a snapshot per
/// ended room so the janitor emits a final `calls.group.state`. Idempotent under
/// the per-task advisory lock.
pub async fn reap_empty_group_rooms(
    pool: &PgPool,
    world: Uuid,
    reap_secs: i64,
) -> anyhow::Result<Vec<CallSnapshot>> {
    let mut tx = world_tx(pool, world).await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('janitor:calls_group_reap'))")
        .execute(&mut *tx)
        .await?;
    let ended: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE call_sessions SET state = 'ended', ended_at = now() \
         WHERE topology = 'sfu' AND state = 'active' \
           AND created_at < now() - make_interval(secs => $1) \
           AND NOT EXISTS (SELECT 1 FROM call_participants p \
                           WHERE p.call_id = call_sessions.id AND p.state = 'joined') \
         RETURNING id",
    )
    .bind(reap_secs as f64)
    .fetch_all(&mut *tx)
    .await?;
    let mut snaps = Vec::with_capacity(ended.len());
    for id in ended {
        snaps.push(group_snapshot_in_tx(&mut tx, id).await?);
    }
    tx.commit().await?;
    Ok(snaps)
}

/// Resolve the world owning `call_id` by walking worlds (call_sessions is
/// FORCE-RLS, so a pool-direct read sees nothing without `app.world_id`). Used
/// only by the webhook, which arrives unauthenticated with just a room name.
/// ponytail: O(worlds) per webhook — fine at v1 world counts; encode the world
/// in the room name if a deploy ever runs many worlds with hot webhooks.
async fn world_of_call(pool: &PgPool, call_id: Uuid) -> anyhow::Result<Option<Uuid>> {
    let worlds: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM worlds")
        .fetch_all(pool)
        .await?;
    for world in worlds {
        let mut tx = world_tx(pool, world).await?;
        let found: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM call_sessions WHERE id = $1 AND topology = 'sfu'")
                .bind(call_id)
                .fetch_optional(&mut *tx)
                .await?;
        tx.commit().await?;
        if found.is_some() {
            return Ok(Some(world));
        }
    }
    Ok(None)
}

/// LiveKit `participant_joined`/`participant_left` truth-sync (G1): mirror the
/// participant row and re-snapshot. Idempotent — setting a row to its current
/// state is a no-op, and a `left` that empties the room ends it. Unknown call
/// (foreign room, already GC'd) → `Ok(None)`, the webhook 200s and ignores.
pub async fn group_webhook_participant(
    pool: &PgPool,
    call_id: Uuid,
    character: Uuid,
    joined: bool,
) -> anyhow::Result<Option<(Uuid, CallSnapshot)>> {
    let Some(world) = world_of_call(pool, call_id).await? else {
        return Ok(None);
    };
    let mut tx = world_tx(pool, world).await?;
    // Lock the session; a concurrent leave/end serializes behind this.
    let exists: Option<i32> =
        sqlx::query_scalar("SELECT 1 FROM call_sessions WHERE id = $1 FOR UPDATE")
            .bind(call_id)
            .fetch_optional(&mut *tx)
            .await?;
    if exists.is_none() {
        tx.commit().await?;
        return Ok(None);
    }
    if joined {
        sqlx::query(
            "INSERT INTO call_participants (call_id, world_id, character_id, state, joined_at) \
             VALUES ($1, $2, $3, 'joined', now()) \
             ON CONFLICT (call_id, character_id) DO UPDATE \
               SET state = 'joined', joined_at = COALESCE(call_participants.joined_at, now())",
        )
        .bind(call_id)
        .bind(world)
        .bind(character)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query(
            "UPDATE call_participants SET state = 'left', left_at = COALESCE(left_at, now()) \
             WHERE call_id = $1 AND character_id = $2",
        )
        .bind(call_id)
        .bind(character)
        .execute(&mut *tx)
        .await?;
        end_if_empty(&mut tx, call_id).await?;
    }
    let snap = group_snapshot_in_tx(&mut tx, call_id).await?;
    tx.commit().await?;
    Ok(Some((world, snap)))
}

/// LiveKit `room_finished` truth-sync (G1): mark the session ended. Idempotent
/// (`state <> 'ended'` guard). Unknown call → `Ok(None)`.
pub async fn group_webhook_room_finished(
    pool: &PgPool,
    call_id: Uuid,
) -> anyhow::Result<Option<(Uuid, CallSnapshot)>> {
    let Some(world) = world_of_call(pool, call_id).await? else {
        return Ok(None);
    };
    let mut tx = world_tx(pool, world).await?;
    sqlx::query(
        "UPDATE call_sessions SET state = 'ended', ended_at = now() \
         WHERE id = $1 AND state <> 'ended'",
    )
    .bind(call_id)
    .execute(&mut *tx)
    .await?;
    let snap = group_snapshot_in_tx(&mut tx, call_id).await?;
    tx.commit().await?;
    Ok(Some((world, snap)))
}
