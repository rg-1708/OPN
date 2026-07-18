-- 0009_calls.sql — Sprint 6 part A (OPN-CORE.md §10.4). Voice/video call
-- sessions with a crash-proof state machine (the FSM is a pure function in
-- primitives/calls/fsm.rs; these tables are just its persistence). Signaling is
-- an opaque relay, never stored. Standard 0001 world-isolation convention
-- (NULLIF form).

-- One call. kind: voice | video; state: ringing | active | ended. The state
-- column is the session half of the FSM; illegal transitions are rejected in
-- the handler (conflict), never written.
CREATE TABLE call_sessions (
    id         uuid NOT NULL PRIMARY KEY,
    world_id   uuid NOT NULL REFERENCES worlds(id),
    kind       text NOT NULL,                       -- voice | video
    state      text NOT NULL DEFAULT 'ringing',     -- ringing | active | ended
    created_at timestamptz NOT NULL DEFAULT now(),
    ended_at   timestamptz
);
-- Janitor zombie-ring reap (§10.4): non-ended sessions by age. Partial so
-- ended calls (the vast majority over time) stay out of the sweep's index.
CREATE INDEX call_sessions_active_age ON call_sessions (created_at)
    WHERE state <> 'ended';

-- One participant of a call. state: ringing | joined | declined | left — the
-- participant half of the FSM. device_id is the device that joined (set on
-- accept; the caller's own at start), NULL while a callee is still ringing.
CREATE TABLE call_participants (
    call_id      uuid NOT NULL REFERENCES call_sessions(id),
    world_id     uuid NOT NULL REFERENCES worlds(id),
    character_id uuid NOT NULL REFERENCES characters(id),
    device_id    uuid,
    state        text NOT NULL DEFAULT 'ringing',   -- ringing | joined | declined | left
    joined_at    timestamptz,
    left_at      timestamptz,
    PRIMARY KEY (call_id, character_id)
);
-- Busy check at calls.start: does this character already hold an active
-- (ringing|joined) participant row? Partial index keyed by character.
CREATE INDEX call_participants_active ON call_participants (character_id)
    WHERE state IN ('ringing', 'joined');

DO $$
DECLARE t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['call_sessions', 'call_participants'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format(
            'CREATE POLICY %I ON %I USING (world_id = NULLIF(current_setting(''app.world_id'', true), '''')::uuid)',
            t || '_world_isolation', t);
        EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %I TO opn_app', t);
    END LOOP;
END $$;
