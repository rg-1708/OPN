-- 0005_channels.sql — Sprint 3 (OPN-CORE.md §10.2, §8, §9).
-- The messaging spine. `messages` is range-partitioned by month on
-- created_at from day one (retrofitting partitioning is a rewrite, §9).
--
-- Standard 0001 world-isolation convention on every table (NULLIF form
-- mandatory). RLS on a partitioned parent is enforced for all access through
-- the parent — which is the only way Core touches messages — so partitions
-- need no policies of their own.

CREATE TABLE channels (
    id         uuid PRIMARY KEY,
    world_id   uuid NOT NULL REFERENCES worlds(id),
    -- sms | group | dm | match | mail (§10.2). open_direct writes 'dm'.
    kind       text NOT NULL,
    name       text,
    meta       jsonb NOT NULL DEFAULT '{}'::jsonb,
    -- Per-channel monotonic message counter; bumped under row lock on send (§8).
    last_seq   bigint NOT NULL DEFAULT 0,
    -- Ordered character pair for open_direct found-or-create (§10.2): the two
    -- member ids sorted (pair_a < pair_b). NULL for groups. No member-set hash.
    pair_a     uuid,
    pair_b     uuid,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- One thread per (kind, ordered pair) per world — the open_direct uniqueness.
CREATE UNIQUE INDEX channels_pair
    ON channels (world_id, kind, pair_a, pair_b) WHERE pair_a IS NOT NULL;

CREATE TABLE channel_members (
    channel_id         uuid NOT NULL REFERENCES channels(id),
    world_id           uuid NOT NULL REFERENCES worlds(id),
    character_id       uuid NOT NULL REFERENCES characters(id),
    joined_at          timestamptz NOT NULL DEFAULT now(),
    -- Watermark receipts (§10.2): never per-message rows. Set in Sprint 4.
    last_delivered_seq bigint NOT NULL DEFAULT 0,
    last_read_seq      bigint NOT NULL DEFAULT 0,
    muted              boolean NOT NULL DEFAULT false,
    PRIMARY KEY (channel_id, character_id)
);

-- channels.list and the send-path offline-member scan both look up by member.
CREATE INDEX channel_members_by_character
    ON channel_members (world_id, character_id);

-- Range-partitioned by month on created_at (§9). Postgres requires the
-- partition key in every unique index, so created_at joins each unique
-- constraint — which is why idempotency dedup cannot rely on the DB unique
-- alone across months (handled in store.rs with a pre-check; see there).
CREATE TABLE messages (
    id               uuid NOT NULL,
    world_id         uuid NOT NULL REFERENCES worlds(id),
    channel_id       uuid NOT NULL,
    seq              bigint NOT NULL,
    sender_character uuid NOT NULL,
    body             jsonb NOT NULL,
    client_uuid      uuid NOT NULL,
    created_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (id, created_at)
) PARTITION BY RANGE (created_at);

CREATE UNIQUE INDEX messages_client_uuid
    ON messages (channel_id, client_uuid, created_at);
CREATE UNIQUE INDEX messages_seq
    ON messages (channel_id, seq, created_at);
-- History reads (Sprint 4) walk seq descending within a channel.
CREATE INDEX messages_channel_seq ON messages (channel_id, seq DESC);

ALTER TABLE messages ENABLE ROW LEVEL SECURITY;
ALTER TABLE messages FORCE ROW LEVEL SECURITY;
CREATE POLICY messages_world_isolation ON messages
    USING (world_id = NULLIF(current_setting('app.world_id', true), '')::uuid);

DO $$
DECLARE t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['channels', 'channel_members'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format(
            'CREATE POLICY %I ON %I USING (world_id = NULLIF(current_setting(''app.world_id'', true), '''')::uuid)',
            t || '_world_isolation', t);
        EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %I TO opn_app', t);
    END LOOP;
END $$;

-- Privilege check for INSERT into a partitioned table is against the parent,
-- so this one grant covers every present and future partition.
GRANT SELECT, INSERT, UPDATE, DELETE ON messages TO opn_app;

-- Partition maintenance runs as opn_app (janitor) but CREATE TABLE needs the
-- table owner. A SECURITY DEFINER function owned by the migrate role bridges
-- that: opn_app may EXECUTE it, the body runs with owner rights. Idempotent
-- (IF NOT EXISTS), so concurrent janitors and re-runs are safe. Sprint 11
-- replaces the caller with pg_cron and deletes the janitor stopgap.
CREATE OR REPLACE FUNCTION ensure_message_partition(target timestamptz)
RETURNS void
LANGUAGE plpgsql
SECURITY DEFINER
-- Pin the search path with pg_temp LAST (PG16 security guidance): opn_app holds
-- the default TEMPORARY privilege, so without this a pg_temp object could
-- shadow an unqualified name (date_trunc/to_char/format/`messages`) and run
-- with the definer's owner rights. `public` stays first so the new partition is
-- created there (not pg_catalog); pg_catalog is still implicitly searched first
-- for built-ins; pg_temp is last, so it can shadow nothing.
SET search_path = public, pg_temp
AS $$
DECLARE
    m0   date := date_trunc('month', target)::date;
    m1   date := (date_trunc('month', target) + interval '1 month')::date;
    part text := 'messages_' || to_char(m0, 'YYYYMM');
BEGIN
    EXECUTE format(
        'CREATE TABLE IF NOT EXISTS %I PARTITION OF messages FOR VALUES FROM (%L) TO (%L)',
        part, m0, m1);
END $$;
REVOKE ALL ON FUNCTION ensure_message_partition(timestamptz) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION ensure_message_partition(timestamptz) TO opn_app;

-- Create this month + next month up front so sends work before the janitor
-- has ticked (migration runs as owner, so a direct call is fine here).
SELECT ensure_message_partition(now());
SELECT ensure_message_partition(now() + interval '1 month');
