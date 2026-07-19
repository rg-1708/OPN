-- 0010_ledger.sql — Sprint 7 part A (OPN-CORE.md §10.5). The ledger: money that
-- cannot be created, destroyed, or double-spent. The load-bearing invariant is
-- that an account is BORN at balance 0 and the ONLY way value ever moves is a
-- `transfers` row (a plain transfer or a hold capture), so for every account
--   balance == Σ(amount WHERE to_account = id) − Σ(amount WHERE from_account = id)
-- holds forever — the equality the nightly reconciliation recomputes and the
-- concurrency battery asserts. Holds reserve spendable balance without moving it
-- (available = balance − Σ held). Standard 0001 world-isolation convention
-- (NULLIF form). Exchange (deposit/withdraw + the `exchanges` table) is part B.

-- An account. owner_kind: character | system. The tenant `system` account is the
-- exchange mint/sink and is the ONLY account allowed to run negative (money that
-- exists in the framework, not in the ledger); every character wallet is bounded
-- ≥ 0 by the CHECK. frozen_at is the reconciliation freeze (§10.5): a frozen
-- account rejects outgoing ops (conflict) until a manual `admin unfreeze`.
CREATE TABLE accounts (
    id              uuid NOT NULL PRIMARY KEY,
    world_id        uuid NOT NULL REFERENCES worlds(id),
    owner_kind      text NOT NULL,                     -- character | system
    owner_character uuid REFERENCES characters(id),    -- NULL for system
    currency        text NOT NULL,
    balance         bigint NOT NULL DEFAULT 0,
    frozen_at       timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now(),
    CHECK (balance >= 0 OR owner_kind = 'system')
);
-- One wallet per (character, currency). System accounts are unconstrained here
-- (one-per-currency is the exchange's concern in part B).
CREATE UNIQUE INDEX accounts_char_wallet
    ON accounts (world_id, owner_character, currency)
    WHERE owner_kind = 'character';

-- An immutable transfer. kind: transfer (a `ledger.transfer`) | capture (a hold
-- settled to a destination). client_uuid is the caller idempotency key for a
-- `transfer` (NULL for a capture — a capture is idempotent via the hold FSM).
-- amount > 0 always; direction is (from_account → to_account).
CREATE TABLE transfers (
    id           uuid NOT NULL PRIMARY KEY,
    world_id     uuid NOT NULL REFERENCES worlds(id),
    from_account uuid NOT NULL REFERENCES accounts(id),
    to_account   uuid NOT NULL REFERENCES accounts(id),
    amount       bigint NOT NULL CHECK (amount > 0),
    kind         text NOT NULL,                         -- transfer | capture
    client_uuid  uuid,
    created_at   timestamptz NOT NULL DEFAULT now()
);
-- Idempotency: a `transfer` retry with the same (from_account, client_uuid)
-- returns the original. Partial so capture rows (client_uuid NULL, which SQL
-- treats as distinct anyway) never participate.
CREATE UNIQUE INDEX transfers_idem
    ON transfers (from_account, client_uuid)
    WHERE client_uuid IS NOT NULL;
-- History keyset (§10.5, CDR-7): a character's own accounts' transfers, newest
-- first. A transfer is "theirs" on either leg, so two directional indexes; the
-- per-account history query BitmapOrs them.
CREATE INDEX transfers_from_created ON transfers (from_account, created_at DESC, id DESC);
CREATE INDEX transfers_to_created   ON transfers (to_account,   created_at DESC, id DESC);

-- A hold: reserved (not moved) balance with an expiry. state: held | captured |
-- released — a 3-state FSM (primitives/ledger/fsm.rs). Only `held` holds count
-- against available balance; the janitor auto-releases expired ones.
CREATE TABLE holds (
    id         uuid NOT NULL PRIMARY KEY,
    world_id   uuid NOT NULL REFERENCES worlds(id),
    account_id uuid NOT NULL REFERENCES accounts(id),
    amount     bigint NOT NULL CHECK (amount > 0),
    state      text NOT NULL DEFAULT 'held',            -- held | captured | released
    expires_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
-- Available-balance sum: Σ amount WHERE account_id = ? AND state = 'held'.
CREATE INDEX holds_account_held ON holds (account_id) WHERE state = 'held';
-- Janitor expiry sweep: held holds past their expiry.
CREATE INDEX holds_expiry ON holds (expires_at) WHERE state = 'held';

DO $$
DECLARE t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['accounts', 'transfers', 'holds'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format(
            'CREATE POLICY %I ON %I USING (world_id = NULLIF(current_setting(''app.world_id'', true), '''')::uuid)',
            t || '_world_isolation', t);
        EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %I TO opn_app', t);
    END LOOP;
END $$;
