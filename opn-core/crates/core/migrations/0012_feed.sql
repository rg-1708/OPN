-- 0012_feed.sql — Sprint 8 (OPN-CORE.md §10.3). The social primitive, built to
-- the fan-out-on-read design. No first-party v1 app consumes it (OPN.md §14.5:
-- the primitive ships, the app is deferred), so this is a from-clean-base slice.
--
-- Feed rows are authored by APP ACCOUNTS, not characters: a post/like/comment/
-- follow is the caller's *active* account for the app (session.app_accounts).
-- Every table is world-scoped FORCE-RLS (0001 NULLIF convention) — the §10.3
-- sketch omitted world_id on likes/comments/hashtags, but the whole codebase
-- makes every domain table world-isolated, and the Sprint 9 RLS audit keys on
-- the world_id column, so all five carry it. (See reflections 2026-07-19,
-- Sprint 8 part A, decision 1.)

-- A post: an app-scoped authored item. body is an opaque app-owned jsonb doc
-- (Core caps size, never interprets — same stance as message bodies); media_ids
-- are validated owned+live at write time, no FK (a later-deleted media renders
-- missing, like a contact avatar). like_count/comment_count are denormalized,
-- bumped atomically in the same tx as the like/comment insert (§10.3) — exact,
-- no drift; the CHECKs are the backstop the guarded decrement never trips.
CREATE TABLE posts (
    id             uuid NOT NULL PRIMARY KEY,
    world_id       uuid NOT NULL REFERENCES worlds(id),
    app_id         text NOT NULL,
    author_account uuid NOT NULL REFERENCES app_accounts(id),
    body           jsonb NOT NULL,
    media_ids      uuid[] NOT NULL DEFAULT '{}',
    like_count     integer NOT NULL DEFAULT 0 CHECK (like_count >= 0),
    comment_count  integer NOT NULL DEFAULT 0 CHECK (comment_count >= 0),
    created_at     timestamptz NOT NULL DEFAULT now()
);
-- Home timeline (§10.3, roadmap item 3): fan-out-on-read walks posts newest
-- first within an app and filters follows per row, so created_at must sit right
-- after the (world, app) equality — this is the index the EXISTS timeline scans
-- (Sprint 8 part B's EXPLAIN test gates on it). The roadmap item-1 index put
-- author_account first, which serves the PROFILE read below, not the home feed;
-- both are needed. (Reflections 2026-07-19, Sprint 8 part A, decision 2.)
CREATE INDEX posts_home ON posts (world_id, app_id, created_at DESC, id DESC);
-- Profile timeline: one author, newest first (roadmap item 1's "timeline" index).
CREATE INDEX posts_author ON posts (world_id, app_id, author_account, created_at DESC, id DESC);

-- A directed follow edge (follower → followee), per app. The PK is also the
-- home-timeline lookup index (§10.3, roadmap item 1).
CREATE TABLE follows (
    world_id         uuid NOT NULL REFERENCES worlds(id),
    app_id           text NOT NULL,
    follower_account uuid NOT NULL REFERENCES app_accounts(id),
    followee_account uuid NOT NULL REFERENCES app_accounts(id),
    created_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (world_id, app_id, follower_account, followee_account)
);

-- A like, one per (post, account). The PK's post_id prefix also serves the
-- cascade delete (delete-by-post) — no separate index needed.
CREATE TABLE likes (
    world_id   uuid NOT NULL REFERENCES worlds(id),
    post_id    uuid NOT NULL REFERENCES posts(id),
    account_id uuid NOT NULL REFERENCES app_accounts(id),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (post_id, account_id)
);

-- A comment on a post. body is an opaque jsonb doc, size-capped like a post.
CREATE TABLE comments (
    id             uuid NOT NULL PRIMARY KEY,
    world_id       uuid NOT NULL REFERENCES worlds(id),
    post_id        uuid NOT NULL REFERENCES posts(id),
    author_account uuid NOT NULL REFERENCES app_accounts(id),
    body           jsonb NOT NULL,
    created_at     timestamptz NOT NULL DEFAULT now()
);
-- Post-detail comment page (cursor idiom, Sprint 8 part B) + the cascade delete.
CREATE INDEX comments_post ON comments (post_id, created_at DESC, id DESC);

-- A hashtag occurrence, parsed server-side at post time (§10.3). The PK is the
-- hashtag-page lookup (world, app, tag → posts); a post_id index serves cascade.
CREATE TABLE hashtags (
    world_id uuid NOT NULL REFERENCES worlds(id),
    app_id   text NOT NULL,
    tag      text NOT NULL,
    post_id  uuid NOT NULL REFERENCES posts(id),
    PRIMARY KEY (world_id, app_id, tag, post_id)
);
CREATE INDEX hashtags_post ON hashtags (post_id);

-- Standard world-isolation RLS (0001 convention, NULLIF form) for all five.
DO $$
DECLARE t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['posts', 'follows', 'likes', 'comments', 'hashtags'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format(
            'CREATE POLICY %I ON %I USING (world_id = NULLIF(current_setting(''app.world_id'', true), '''')::uuid)',
            t || '_world_isolation', t);
        EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %I TO opn_app', t);
    END LOOP;
END $$;
