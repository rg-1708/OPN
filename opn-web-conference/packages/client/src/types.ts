import type { ServerMsg } from "@opn/contracts";

/**
 * Connection lifecycle as the app sees it (OPN.md §7, roadmap W0).
 *
 * - `connecting`   — first WS open + auth handshake in flight
 * - `live`         — authed; commands and pushes flow
 * - `reconnecting` — dropped; backing off, will re-auth + resume
 * - `taken_over`   — a newer session on the same identity won (close `4408`);
 *                    terminal, no reconnect — the app must surface it
 * - `closed`       — terminal (user closed, or unrecoverable auth loss)
 */
export type ConnectionState =
  | "connecting"
  | "live"
  | "reconnecting"
  | "taken_over"
  | "closed";

/** The push arm of `ServerMsg` (`{ topic, evt, payload }`). */
export type Push = Extract<ServerMsg, { topic: string }>;
/** The ack arm of `ServerMsg` (`{ reply_to, ok, payload?, err? }`). */
export type Ack = Extract<ServerMsg, { reply_to: number }>;
/** A successful ack's payload (`JsonValue | undefined`). */
export type AckPayload = Ack["payload"];

/** A per-topic push handler. Receives the whole push so `evt` narrows `payload`. */
export type PushHandler = (push: Push) => void;

/**
 * A WS-ish object the connection drives. Both the browser `WebSocket` and
 * Node 22+'s global `WebSocket` satisfy it structurally; tests inject a fake.
 * Deliberately the `on*`-property surface (not `addEventListener`) — smallest
 * thing both real impls and a 30-line fake can implement.
 */
export interface WebSocketLike {
  readonly readyState: number;
  send(data: string): void;
  close(code?: number, reason?: string): void;
  onopen: ((ev: unknown) => void) | null;
  onmessage: ((ev: { data: unknown }) => void) | null;
  onclose: ((ev: { code: number; reason: string }) => void) | null;
  onerror: ((ev: unknown) => void) | null;
}

/** Injectable timers + randomness so tests drive backoff/refresh deterministically. */
export interface Scheduler {
  setTimeout(fn: () => void, ms: number): unknown;
  clearTimeout(handle: unknown): void;
  /** Uniform [0, 1). Only used for reconnect jitter. */
  random(): number;
}

export interface ClientOptions {
  /** `ws(s)://host/ws` — the Core gateway. */
  url: string;
  /** Initial session JWT (minted out-of-band via the dev-auth sidecar). */
  token: string;
  /**
   * Re-mint a fresh JWT after an unrecoverable auth loss (the session died, so
   * `auth.refresh` can't heal it). In the template this calls the dev-auth
   * `/join` endpoint again. Absent → a `4401`/expired session is terminal.
   */
  remint?: () => Promise<string>;
  /** Called with every fresh token (refresh or remint) — persist it + use it for HTTP. */
  onToken?: (token: string) => void;
  /** JWT / session lifetime in ms. Refresh fires at `ttl - skew`. Default 600_000 (10 min). */
  sessionTtlMs?: number;
  /** How far before expiry to refresh. Default 60_000. */
  refreshSkewMs?: number;
  /** Per-command ack timeout in ms. Default 10_000. */
  ackTimeoutMs?: number;
  /** Reconnect jitter ceiling in ms (full jitter, 0..max). Default 3_000 (OPN.md §7). */
  maxBackoffMs?: number;
  /** Dedupe ring size per channel topic. Default 512. */
  dedupeRingSize?: number;
  /** Test seam: build the socket. Default `new WebSocket(url)`. */
  socketFactory?: (url: string) => WebSocketLike;
  /** Test seam: timers + randomness. Default wraps `globalThis`. */
  scheduler?: Scheduler;
}

export const defaultScheduler: Scheduler = {
  setTimeout: (fn, ms) => globalThis.setTimeout(fn, ms),
  clearTimeout: (h) => globalThis.clearTimeout(h as ReturnType<typeof setTimeout>),
  random: () => Math.random(),
};
