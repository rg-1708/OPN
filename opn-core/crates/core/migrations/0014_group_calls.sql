-- 0014_group_calls.sql — Sprint G1 (opn-group-calls.md). Group voice calls
-- reuse the Sprint 6 call_sessions/call_participants tables (already
-- N-participant shaped); this migration only adds the two SFU columns.
--
-- topology: p2p (1:1 calls, media peer-to-peer, Core relays signaling) | sfu
-- (group calls, media forwards through the LiveKit sidecar). Default 'p2p' so
-- every existing 1:1 row reads correctly with no backfill. sfu_room_id is the
-- LiveKit room name ("grp_<call id>"), NULL for p2p sessions.
ALTER TABLE call_sessions
    ADD COLUMN topology    text NOT NULL DEFAULT 'p2p',   -- p2p | sfu
    ADD COLUMN sfu_room_id text;                          -- LiveKit room, NULL for p2p

-- Janitor empty-group-room reap (opn-group-calls.md G1): active SFU sessions by
-- age. Partial index keyed to the sweep's exact predicate (topology, state).
CREATE INDEX call_sessions_group_active ON call_sessions (created_at)
    WHERE topology = 'sfu' AND state = 'active';
