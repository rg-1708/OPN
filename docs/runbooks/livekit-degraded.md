# Runbook: LiveKit degraded / down (group calls)

## Current state (verified)

Group voice calls (docs/opn-group-calls.md) fail **closed and isolated**:
LiveKit is a sidecar SFU; Core only mints join tokens and consumes its
webhooks. When `LIVEKIT_*` env is unset/empty the whole feature is off. When
LiveKit is down, `calls.group.*` joins hand out tokens nobody can redeem and
new media stops — but **1:1 calls (P2P/coturn) and the entire data plane are
untouched**. There is no Core code path that blocks on LiveKit availability;
webhooks are inbound-only.

Blast radius of a LiveKit outage = group-call audio only.

## Symptoms

- Clients join a group call (snapshot shows them `joined`) but hear nothing /
  SFU connect fails.
- `opn_livekit_webhook_total` flatlines while group calls are supposedly
  active.
- Participant lists drift from reality (webhook truth-sync missing) until the
  janitor reaps empty rooms (`LIVEKIT_EMPTY_ROOM_REAP_SECS`, default 300).

## Triage

1. Is the container up?
   ```bash
   docker compose -f docker-compose.prod.yml ps livekit
   docker compose -f docker-compose.prod.yml logs --tail 50 livekit
   ```
2. Signal reachable? `curl -fsS https://livekit.<domain>` (Traefik-routed
   7880; TLS/router problems show here, not in the livekit log).
3. Media reachable? UDP 50000–50060 open on the host firewall; `rtc.use_external_ip`
   needs the host's public IP discoverable. Symptom split: signal connects
   but silence → media/UDP; nothing connects → signal/Traefik.
4. Webhook auth failing? Core log lines for rejected webhook signatures →
   key/secret mismatch between `OPN_LIVEKIT_API_KEY/SECRET` (Core env) and
   the `keys:` block in the livekit service. They are the same secret store
   values by construction — a mismatch means a half-applied redeploy.

## Recovery

- Restart is safe and stateless: `docker compose -f docker-compose.prod.yml
  restart livekit`. In-flight group calls drop and clients rejoin (fresh
  token via `calls.group.join`); Core state self-heals via webhooks + janitor.
- Version pin is deliberate (`livekit/livekit-server:v1.13.4`). Upgrade =
  change the pin, redeploy, verify a 3-way call on the dev stack first
  (`docker-compose.dev.yml` carries the same service).
- If LiveKit stays down and noise matters: unset `OPN_LIVEKIT_URL` (or all
  three vars) and redeploy — group commands then answer `forbidden`
  immediately instead of minting dead tokens. Re-set to re-enable; no
  migration or data implications either way.

## Not this runbook

- 1:1 call problems → coturn/ICE (`OPN_ICE_SERVERS`), see incident-triage.md.
- Group-call rows stuck `active` with zero participants → janitor; check
  Core log for the reap task, not LiveKit.
