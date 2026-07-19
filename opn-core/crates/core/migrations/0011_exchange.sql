-- 0011_exchange.sql — Sprint 7 part B (OPN-CORE.md §10.5, OPN.md §14.2). The
-- framework exchange: the ONLY seam where value crosses between the framework
-- bank and the ledger. Two directions, both idempotent and audited:
--   * deposit          — the bridge credits a character wallet from the tenant
--                        `system` account (system → wallet, transfer kind='deposit').
--   * withdraw         — two-legged. Leg 1 (WS `ledger.withdraw`) holds the wallet
--                        + writes a `pending_confirm` exchange row; leg 2 (the
--                        bridge's `withdraw_confirm`) captures the hold to system
--                        (wallet → system, transfer kind='withdraw').
-- The distinct transfer `kind`s ('deposit'/'withdraw') are load-bearing: the
-- nightly reconciliation cross-checks Σ(exchanges) against exactly those legs, so
-- a genuine transfer/capture (kind 'transfer'/'capture') never trips it and an
-- orphaned exchange row (or a missing leg) freezes the system account.

-- Per-currency tenant config (§10.5 item 4 "currency from tenant config"). One
-- tenant per world (admin.rs enforces it), so this is effectively per-world. New
-- column with a default so existing tenants keep working; grant the runtime role
-- read on just this column (tenants is not RLS-scoped — column grants are the
-- access control, per 0003).
ALTER TABLE tenants ADD COLUMN currency text NOT NULL DEFAULT 'OPN';
GRANT SELECT (currency) ON tenants TO opn_app;

-- One system account per (world, currency) — the exchange mint/sink. The part-A
-- accounts_char_wallet index constrained character wallets; system needs its own
-- so the deposit/confirm get-or-create can `ON CONFLICT` on it (idempotent under
-- concurrent first-touch).
CREATE UNIQUE INDEX accounts_system
    ON accounts (world_id, currency)
    WHERE owner_kind = 'system';

-- An exchange event: idempotency key + audit for one deposit or withdraw. `id` is
-- bridge-chosen for a deposit (the framework's own ref) and Core-minted for a
-- withdraw (returned to the client, relayed to the bridge to confirm). PK
-- (world_id, id) since the id is only unique within a world.
--   direction: deposit | withdraw
--   state:     done            — a completed deposit, or a confirmed withdraw
--              pending_confirm  — a withdraw awaiting the bridge's confirm
--              expired          — a withdraw whose hold expired unconfirmed
--   hold_id:   the wallet hold backing a withdraw (NULL for a deposit); the
--              janitor flips the exchange to 'expired' when this hold auto-releases.
CREATE TABLE exchanges (
    world_id     uuid NOT NULL REFERENCES worlds(id),
    id           text NOT NULL,
    character_id uuid NOT NULL REFERENCES characters(id),
    amount       bigint NOT NULL CHECK (amount > 0),
    direction    text NOT NULL,                          -- deposit | withdraw
    state        text NOT NULL,                          -- done | pending_confirm | expired
    hold_id      uuid REFERENCES holds(id),
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (world_id, id)
);
-- Journal keyset for the bridge's reconciliation feed (`GET .../exchange?since`),
-- oldest-first on (created_at, id).
CREATE INDEX exchanges_journal ON exchanges (world_id, created_at, id);
-- Janitor: flip pending_confirm withdraws to expired when their hold releases.
CREATE INDEX exchanges_pending_hold ON exchanges (hold_id) WHERE state = 'pending_confirm';

ALTER TABLE exchanges ENABLE ROW LEVEL SECURITY;
ALTER TABLE exchanges FORCE ROW LEVEL SECURITY;
CREATE POLICY exchanges_world_isolation ON exchanges
    USING (world_id = NULLIF(current_setting('app.world_id', true), '')::uuid);
GRANT SELECT, INSERT, UPDATE, DELETE ON exchanges TO opn_app;
