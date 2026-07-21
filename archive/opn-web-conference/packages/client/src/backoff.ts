/**
 * Reconnect delay: full jitter, uniform in [0, maxMs) (OPN.md §7 "0–3 s jitter").
 *
 * Full jitter, not exponential-with-jitter: the storm the server guards against
 * is a thundering herd reconnecting at the same instant a Core restart drops
 * every socket, and a flat 0–3 s spread already de-syncs them. A single dev
 * Core has no cause to punish a client for reconnecting twice.
 *
 * ponytail: flat jitter; add exponential ceiling growth only if repeated
 * reconnects are seen hammering a genuinely-down Core.
 */
export function backoffMs(maxMs: number, random: () => number): number {
  const ceil = Math.max(0, maxMs);
  return Math.floor(random() * ceil);
}
