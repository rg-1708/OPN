# Runbook: rotating `OPN_JWT_SECRET`

## Current state (verified)

Session JWTs are **HS256**, TTL **600 s / 10 min** (`JWT_TTL_SECS`,
[auth.rs:41](../../opn-core/crates/core/src/infra/auth.rs)). Minted with
`mint_jwt(secret, …)` ([auth.rs:43](../../opn-core/crates/core/src/infra/auth.rs));
verified with `verify(pool, secret, token)` against a **single** secret —
`DecodingKey::from_secret(secret.as_bytes())` + `Validation::default()`
([auth.rs:114](../../opn-core/crates/core/src/infra/auth.rs)). The secret comes from
the required env var `OPN_JWT_SECRET` ([config.rs:98](../../opn-core/crates/core/src/config.rs)).
Clients refresh a live token over the open WS with the `auth.refresh` command
(`Cmd::AuthRefresh`), so a token rides ~10 min and is renewed in place, not on reconnect.

**Consequence:** one secret verifies. Swapping it invalidates every token minted under
the old one immediately — there is no overlap window in the current code.

## Procedure (current, safe path)

Naive swap-and-redeploy. Accept a bounded reconnect storm; do it off-peak.

1. Generate a new secret (high-entropy):
   ```bash
   openssl rand -base64 48
   ```
2. Set `OPN_JWT_SECRET` to the new value across **all** instances and redeploy.
3. On restart, every in-flight token (signed with the old secret) fails `verify` →
   those connections are rejected. Clients re-establish from their durable credential
   (API key / session) and re-mint a token, then resume.

**Blast radius:** a reconnect + re-mint storm bounded by the 10-min TTL — worst case is
every currently-connected client reconnecting once. Persist-then-ack + resume/inbox mean
no message loss, only a brief live-push gap per client. Mitigate by rotating during a
low-traffic window; client auto-reconnect does the rest. If you rotate instances one at
a time, tokens are rejected as soon as the **first** instance has the new secret (a
token minted on an old-secret instance won't verify on a new-secret one), so a rolling
deploy does not avoid the storm — it smears it across the rollout.

## Zero-downtime dual-secret overlap — **(planned — not yet implemented)**

The overlap-free rotation everyone wants: **mint with the new secret, verify against
either** (new *or* previous) for one deploy, then drop the old secret on the next
deploy. Live tokens keep verifying through the window, so no reconnect storm.

This needs a code change: `verify` currently takes exactly one `secret`
([auth.rs:114](../../opn-core/crates/core/src/infra/auth.rs)) and there is no
"previous secret" config var. Implementing it means (a) a second optional env var
(e.g. `OPN_JWT_SECRET_PREV`), (b) `verify` trying the primary then the previous key,
and (c) `mint_jwt` always using the primary. Until that lands, use the swap-and-redeploy
path above.
