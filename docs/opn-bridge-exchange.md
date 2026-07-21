# OPN Exchange Protocol — Bridge Author's Guide

The **exchange** is the one seam where value crosses between the framework's own
economy (the game's bank / `money` table) and the OPN ledger. It exists so an
in-game wallet app can hold OPN balances that a player can move *into* and *out
of* the framework at will, without either side ever being able to create or
destroy money silently.

This document is the contract for the **bridge** — the framework-side code
(other repo) that talks to Core's HTTP exchange endpoints. It is normative:
Core's behavior is exactly what is written here. Companion to
[OPN-CORE.md §10.5](OPN-CORE.md) (design).

---

## 0. Model in one paragraph

Every world has a tenant `system` account — the mint/sink. It is the only
account allowed to run a negative balance: a negative `system` balance is
exactly the amount of money that currently lives in the ledger but not in the
framework bank (players deposited it). A **deposit** moves money `system →
wallet` (the framework debited its own bank first). A **withdraw** moves money
`wallet → system` (the framework credits its own bank after). Both are recorded
as immutable `transfers` rows *and* audited by an `exchanges` row; a nightly
reconciliation cross-checks the two and **freezes the system account** if they
ever disagree, which halts all exchange flow until an operator investigates.

**Amounts are integers** in the currency's minor unit (e.g. cents). No floats,
ever. Every amount must be `> 0`.

**Currency** is per-tenant (`tenants.currency`, default `OPN`). One tenant per
world today, so it is effectively per-world. The bridge never sends a currency —
Core reads it from the tenant.

---

## 1. Authentication

All exchange HTTP calls are authenticated with the tenant **API key**, exactly
like every other `/v1/tenants/self/*` route:

```
Authorization: Bearer opn_<your-api-key>
```

A missing/unknown key → `401`. The key scopes every call to the tenant's world;
you cannot touch another world's ledger.

Base path: `POST` and `GET` on `/v1/tenants/self/exchange`.

---

## 2. Deposit (framework → ledger)

The player asks (in the framework UI) to move money into their OPN wallet. The
bridge debits the framework bank **first**, then calls:

```
POST /v1/tenants/self/exchange
{
  "exchange_id":  "<your unique id for this deposit>",
  "character_id": "<uuid of the target character>",
  "amount":       500,
  "direction":    "deposit"
}
```

- `exchange_id` is **bridge-chosen** and is the **idempotency key**. Use your own
  bank-transaction id. Re-POSTing the same `exchange_id` returns the stored
  result and moves **no** additional money — retry freely on timeout/network
  failure.
- On first sight, Core auto-creates the character's wallet (and the world's
  `system` account) if they don't exist yet, then moves `amount` from `system`
  to the wallet and notifies the character (in-app "money received").

**Response `200`:**
```json
{ "exchange_id": "<echoed>", "state": "done", "amount": 500 }
```

**Errors:**

| Status | Meaning |
|---|---|
| `400` | `amount <= 0`, or `exchange_id` empty / > 128 chars |
| `404` | `character_id` is not a character in this world |
| `409` | the `system` account is **frozen** (reconciliation detected drift — stop and page an operator) |

**Ordering rule:** debit the framework bank *before* the deposit call. If the
deposit call fails after your bank debit, retry with the **same** `exchange_id`
until it succeeds — it is idempotent, so at-least-once retry is safe.

---

## 3. Withdraw (ledger → framework) — two legs

A withdraw is two-legged because the money must be *reserved* in the ledger
while the framework credits its own bank, so a crash on either side can't
double-spend.

### Leg 1 — the in-game app starts it (WS, not your call)

The wallet app (running in the OPN shell) sends a WebSocket command:

```
{ "cmd": "ledger.withdraw", "payload": { "amount": 400 } }
```

Core places a **hold** on the player's wallet (the 400 is now excluded from
their spendable balance but not yet moved) and opens a `pending_confirm`
exchange, replying:

```json
{ "exchange_id": "<Core-minted uuid>" }
```

The app relays this `exchange_id` to **you** over the game plane (whatever
in-game event channel you already use). You now know a withdraw is pending.

If the wallet has insufficient available balance (or none), Core rejects leg 1
with `conflict` and you never hear about it — nothing to do.

### Leg 2 — you confirm it

Credit the framework bank **first**, then confirm so Core settles the hold:

```
POST /v1/tenants/self/exchange
{
  "exchange_id":  "<the Core-minted id from leg 1>",
  "character_id": "<same character>",
  "amount":       400,
  "direction":    "withdraw_confirm"
}
```

Core captures the hold to `system` (money leaves the ledger) and marks the
exchange `done`.

**Response `200`:**
```json
{ "exchange_id": "<echoed>", "state": "done", "amount": 400 }
```

`withdraw_confirm` is **idempotent** on the terminal `done` state — re-POST after
a timeout and you get `done` again with no second capture.

**Errors:**

| Status | Meaning |
|---|---|
| `400` | `character_id` or `amount` doesn't match the stored exchange |
| `404` | no `withdraw` exchange with that `exchange_id` in this world |
| `409` | the exchange **expired** (see below), or the wallet is frozen |

### Expiry

If you never confirm, the wallet hold expires after **1 hour**. A janitor then
releases the hold (the player's balance is freed) and flips the exchange to
`expired`. A `withdraw_confirm` on an expired exchange returns `409` — at that
point you must **not** have credited the framework bank (or must reverse it).

**Rule of thumb:** only credit the framework bank once you are committed to
confirming immediately. If you're going to be slow, credit *after* a successful
confirm instead — but then treat a `409 expired` as "the ledger already gave the
money back, do nothing."

---

## 4. Reconciliation journal (your safety net)

```
GET /v1/tenants/self/exchange?since=<RFC3339 timestamp>&limit=<n≤500>
```

Returns the world's exchanges, **oldest first**, from `since` (inclusive):

```json
{
  "items": [
    { "id": "dep-1", "character_id": "…", "amount": 500,
      "direction": "deposit",  "state": "done",     "created_at": "2026-07-19T03:00:00Z" },
    { "id": "…",     "character_id": "…", "amount": 400,
      "direction": "withdraw", "state": "pending_confirm", "created_at": "…" }
  ]
}
```

Poll this to reconcile against your own bank ledger. Advance `since` to the last
row's `created_at` and **dedupe by `id`** — `since` is inclusive, so the boundary
timestamp may re-appear; re-reading it is safe and no row is ever skipped.
Omit `since` to read from the beginning. `limit` defaults to 100, caps at 500.

`state` values: `done`, `pending_confirm`, `expired`. `direction`: `deposit`,
`withdraw`.

---

## 5. What Core does on its side (so you can trust it)

Core runs a **nightly reconciliation** that, per world:

1. Recomputes every account's balance from its immutable `transfers` and freezes
   any account that drifted (money changed outside a transfer).
2. **Cross-checks the exchange journal against the ledger:** the sum of every
   `done` deposit must equal the sum of the `deposit` transfer legs, and the same
   for withdraws. If a deposit/withdraw exchange row ever lost (or gained) its
   money leg — the kind of corruption a balance recompute alone can't see — Core
   **freezes the `system` account** and logs `opn_ledger_drift_total`.

A frozen `system` account makes every subsequent `deposit`/`withdraw_confirm`
return `409`. That is the system telling you money integrity is in doubt: **stop
exchanging and page an operator.** Unfreezing is a deliberate manual step
(`opn-core admin unfreeze`) after a human reconciles the books — never automatic.

This is why the protocol is worth the two-leg dance: at no point can a crash,
retry, or dropped message move money twice or lose it silently — and if anything
ever does, it is detected within a day and stops the bleeding.

---

## 6. Quick reference

| Action | Who calls | Endpoint / command | Idempotency key |
|---|---|---|---|
| Deposit | bridge (HTTP) | `POST .../exchange` `direction:deposit` | `exchange_id` (you choose) |
| Withdraw leg 1 | in-game app (WS) | `ledger.withdraw` | — (Core mints the id) |
| Withdraw leg 2 | bridge (HTTP) | `POST .../exchange` `direction:withdraw_confirm` | `exchange_id` (from leg 1) |
| Reconcile | bridge (HTTP) | `GET .../exchange?since` | — |

All amounts positive integers, minor units. All HTTP calls `Authorization:
Bearer opn_…`. Retry on `5xx`/timeout with the same body — every write is
idempotent.
