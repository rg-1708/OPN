# Runbook: frozen (reconciliation-frozen) account

A frozen account is the ledger's silent-corruption tripwire, not a routine state.
Treat every freeze as a money-integrity incident.

## Mechanism

The nightly janitor task `ledger_reconcile`
([janitor.rs:235](../../opn-core/crates/core/src/janitor.rs)) runs once a day —
gated to `OPN_RECONCILE_HOUR` (UTC hour, default `3` = 03:00 UTC;
[config.rs:83](../../opn-core/crates/core/src/config.rs)). The janitor loop still
ticks every 30 s but the recompute only fires during that hour. Per world it calls
`store::reconcile` ([store.rs:478](../../opn-core/crates/core/src/primitives/ledger/store.rs)),
which, for the ledger invariant `balance == Σ(amount WHERE to_account=id) − Σ(amount WHERE from_account=id)`:

- freezes every account whose stored `balance` disagrees with that sum —
  `UPDATE accounts SET frozen_at = now() WHERE frozen_at IS NULL AND balance <> (…)`;
- runs the exchange cross-check, which freezes the world's `system` account if
  `Σ(done exchanges)` disagrees with its deposit/withdraw legs
  ([exchange.rs:517](../../opn-core/crates/core/src/primitives/ledger/exchange.rs));
- increments `opn_ledger_drift_total` by the number frozen and logs at ERROR:
  `ledger reconciliation froze drifted accounts — silent corruption detected`
  ([store.rs:504](../../opn-core/crates/core/src/primitives/ledger/store.rs)).

A frozen account **rejects all outgoing operations** (transfer/hold/capture) with a
`conflict` ack — `Fail::Code(ErrCode::Conflict) // frozen source`
([store.rs:143](../../opn-core/crates/core/src/primitives/ledger/store.rs); the
invariant is documented at [migrations/0010_ledger.sql:15](../../opn-core/crates/core/migrations/0010_ledger.sql)).
Incoming transfers still land. The freeze is idempotent (`frozen_at IS NULL` guard).

## Detect

Alert signal: `opn_ledger_drift_total > 0`, or the ERROR log line above. To enumerate,
connect as the **owner role** (`OPN_MIGRATE_DATABASE_URL`, bypasses RLS — scope the
world explicitly):

```bash
psql "$OPN_MIGRATE_DATABASE_URL" -c \
  "SELECT id, world_id, balance, frozen_at FROM accounts WHERE frozen_at IS NOT NULL;"
```

## Diagnose — stored vs. recomputed

Same query the reconciler uses; the two columns should match for a *correct* account:

```sql
SELECT a.id, a.world_id, a.balance,
       COALESCE((SELECT SUM(amount) FROM transfers WHERE to_account   = a.id), 0)
     - COALESCE((SELECT SUM(amount) FROM transfers WHERE from_account = a.id), 0) AS recomputed
FROM accounts a
WHERE a.frozen_at IS NOT NULL;
```

`balance <> recomputed` confirms drift. A frozen `owner_kind='system'` account means
the exchange cross-check tripped — audit the `exchanges` table against its
deposit/withdraw legs before thawing. **Do not thaw until a human has established the
true balance and root cause.** Freeze means money moved without a matching leg (or
vice-versa); un-freezing without fixing the underlying rows re-enables spending on a
wrong balance.

## Recover

After a human confirms the true balance, thaw with the admin CLI
([admin.rs:126](../../opn-core/crates/core/src/admin.rs)) — it connects as the owner
role via `OPN_MIGRATE_DATABASE_URL`, clears `frozen_at` **only** on a currently-frozen
account, and reports a no-op otherwise:

```bash
opn-core admin unfreeze --world <world-uuid> --account <account-uuid>
# → "unfroze account <uuid> in world <uuid>"
# → bails "no frozen account <uuid> in world <uuid> (already thawed or absent)" if not frozen
```

If the stored `balance` was wrong, correct it (post an adjusting `transfers` row via
the normal ledger path so the invariant holds) **before** unfreezing — thaw does not
touch `balance`.

> **Break-glass only.** Hand-running `UPDATE accounts SET frozen_at = NULL …` bypasses
> the owner-role/world scoping and the currently-frozen guard, and leaves no operator
> record. Use it only if the CLI binary is unavailable mid-incident, with the same
> `world_id AND id AND frozen_at IS NOT NULL` predicate the CLI uses, and note it in
> the incident log.
