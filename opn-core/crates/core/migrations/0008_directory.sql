-- 0008_directory.sql — Sprint 5 part B (OPN-CORE.md §10.7). The directory:
-- contacts (a character's private address book), blocks (block-by-number), and
-- listings (YellowPages/ads with a TTL). Numbers resolve to a character only at
-- action time via directory::resolve, which also filters blocked pairs — the
-- number→character map never crosses the wire. Standard 0001 world-isolation
-- convention (NULLIF form).

-- A character's private contacts. Contacts point at raw numbers (PK
-- (owner_character, number)); resolution to a live character happens only at
-- action time (open_direct/calls.start), never here. avatar_media is validated
-- owned-and-live by the handler when present.
CREATE TABLE contacts (
    owner_character uuid NOT NULL REFERENCES characters(id),
    world_id        uuid NOT NULL REFERENCES worlds(id),
    number          text NOT NULL,
    display_name    text NOT NULL,
    -- No FK to media(id) on purpose — same as message attachments (media_ids in
    -- a message body carry no FK): ownership+live is validated at write time,
    -- and a later-deleted media just renders as a missing avatar. An FK would
    -- instead make the media janitor's DELETE fail while a contact references a
    -- reverted/reaped row.
    avatar_media    uuid,
    meta            jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (owner_character, number)
);

-- Block-by-number (§10.7). Enforced at the action points (open_direct,
-- calls.start) via directory::resolve, which returns None for a blocked pair so
-- a block is indistinguishable from an unknown number (privacy). blocked_number
-- is free-form on purpose — you may block a number that isn't (yet) a character.
CREATE TABLE blocks (
    blocker_character uuid NOT NULL REFERENCES characters(id),
    world_id          uuid NOT NULL REFERENCES worlds(id),
    blocked_number    text NOT NULL,
    created_at        timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (blocker_character, blocked_number)
);
-- resolve()'s reverse-direction check: "did the callee block the caller's
-- number?" is a lookup by (blocker_character, blocked_number) — the PK already
-- serves it. The forward direction (caller blocked callee) is the same PK.

-- Listings: app-scoped ads/postings with an optional TTL. owner_character is a
-- deviation from the §10.7 tuple (which omits it): CRUD needs an owner to scope
-- delete and a "my listings" view — see reflections 2026-07-18 (Sprint 5 B).
CREATE TABLE listings (
    id              uuid NOT NULL PRIMARY KEY,
    world_id        uuid NOT NULL REFERENCES worlds(id),
    owner_character uuid NOT NULL REFERENCES characters(id),
    app_id          text NOT NULL,
    kind            text NOT NULL,
    title           text NOT NULL,
    body            jsonb NOT NULL DEFAULT '{}'::jsonb,
    contact_number  text NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    -- NULL = never expires. The janitor deletes rows past expiry.
    expires_at      timestamptz
);
-- Listing reads (directory.listings): active rows for an app, newest-first on
-- the cursor idiom (created_at, id).
CREATE INDEX listings_app_feed ON listings (world_id, app_id, created_at DESC, id DESC);
-- Janitor expiry sweep: expiring rows by expiry time (partial — never-expiring
-- rows stay out of the sweep's index).
CREATE INDEX listings_expiry ON listings (expires_at) WHERE expires_at IS NOT NULL;

DO $$
DECLARE t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['contacts', 'blocks', 'listings'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format(
            'CREATE POLICY %I ON %I USING (world_id = NULLIF(current_setting(''app.world_id'', true), '''')::uuid)',
            t || '_world_isolation', t);
        EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %I TO opn_app', t);
    END LOOP;
END $$;
