import type { Cmd } from "@opn/contracts";
import { backoffMs } from "./backoff.ts";
import { DedupeRing } from "./dedupe.ts";
import { OpnError } from "./errors.ts";
import { PendingMap } from "./pending.ts";
import {
  defaultScheduler,
  type Ack,
  type AckPayload,
  type ClientOptions,
  type ConnectionState,
  type Push,
  type PushHandler,
  type Scheduler,
  type WebSocketLike,
} from "./types.ts";

/** Gateway close codes (OPN-CORE.md §4.1, `gateway/registry.rs`). */
const CLOSE = {
  badFirstFrame: 4400,
  unauthorized: 4401,
  takenOver: 4408,
  slowConsumer: 4409,
} as const;

interface SubEntry {
  /** Highest seq seen on this topic, replayed as `last_seq` on resume. `null` for seqless topics. */
  lastSeq: number | null;
}

/**
 * The OPN wire runtime (roadmap W0). One connection = one authenticated,
 * self-healing session to a stock Core: first-frame auth, cmd/ack correlation,
 * `auth.refresh`, reconnect with jitter, resubscribe-with-`last_seq` resume, and
 * per-topic push dispatch with `message_id` dedupe. React-free and style-free.
 */
export class OpnConnection {
  readonly #opts: ClientOptions;
  readonly #scheduler: Scheduler;
  readonly #socketFactory: (url: string) => WebSocketLike;
  readonly #pending: PendingMap;

  #ws: WebSocketLike | null = null;
  #state: ConnectionState = "connecting";
  #nextId = 1;
  #token: string;
  #closedByUser = false;

  readonly #stateListeners = new Set<(s: ConnectionState) => void>();
  readonly #topicHandlers = new Map<string, Set<PushHandler>>();
  readonly #subs = new Map<string, SubEntry>();
  readonly #dedupe = new Map<string, DedupeRing>();

  #refreshTimer: unknown = null;
  #reconnectTimer: unknown = null;

  constructor(opts: ClientOptions) {
    this.#opts = opts;
    this.#scheduler = opts.scheduler ?? defaultScheduler;
    this.#socketFactory =
      opts.socketFactory ?? ((url) => new WebSocket(url) as unknown as WebSocketLike);
    this.#pending = new PendingMap(this.#scheduler, opts.ackTimeoutMs ?? 10_000);
    this.#token = opts.token;
    this.#open(false);
  }

  // ── public surface ────────────────────────────────────────────────────────

  get state(): ConnectionState {
    return this.#state;
  }

  /** The current session JWT — updated by refresh/remint; use it for HTTP calls. */
  get token(): string {
    return this.#token;
  }

  /** Subscribe to connection-state changes. Returns an unsubscribe fn. */
  onState(cb: (state: ConnectionState) => void): () => void {
    this.#stateListeners.add(cb);
    return () => this.#stateListeners.delete(cb);
  }

  /**
   * Register a push handler for `topic`. Does NOT subscribe — call `sub(topic)`
   * to make Core start sending. Returns an unsubscribe fn (handler only).
   */
  on(topic: string, handler: PushHandler): () => void {
    let set = this.#topicHandlers.get(topic);
    if (!set) {
      set = new Set();
      this.#topicHandlers.set(topic, set);
    }
    set.add(handler);
    return () => {
      const s = this.#topicHandlers.get(topic);
      s?.delete(handler);
      if (s && s.size === 0) this.#topicHandlers.delete(topic);
    };
  }

  /**
   * Send a command and await its ack. Resolves with the ack payload (`unknown`
   * until the command has a typed shape); rejects with `OpnError` on `ok:false`,
   * timeout, or a drop. Rejects `not_connected` if not `live` — the app owns
   * retry (resend the same `client_uuid` on reconnect, roadmap W1).
   */
  cmd(cmd: Cmd): Promise<AckPayload> {
    if (this.#state !== "live" || !this.#ws) {
      return Promise.reject(
        new OpnError("not_connected", `cannot send "${cmd.cmd}" while ${this.#state}`),
      );
    }
    const id = this.#nextId++;
    const promise = this.#pending.await(id);
    this.#rawSend({ id, ...cmd });
    return promise;
  }

  /**
   * Subscribe to `topic`, tracking it so a reconnect re-subscribes with the
   * resume watermark. `lastSeq` seeds the watermark (e.g. after a cold history
   * load); live channel messages advance it automatically.
   */
  async sub(topic: string, lastSeq: number | null = null): Promise<void> {
    const existing = this.#subs.get(topic);
    const seq = existing?.lastSeq ?? lastSeq;
    this.#subs.set(topic, { lastSeq: seq });
    try {
      await this.cmd({ cmd: "sub", payload: { topic, last_seq: seq } });
    } catch (err) {
      // A failed sub (e.g. `forbidden`) must not keep re-firing every reconnect.
      this.#subs.delete(topic);
      throw err;
    }
  }

  /** Unsubscribe and stop tracking `topic` for resume. */
  async unsub(topic: string): Promise<void> {
    this.#subs.delete(topic);
    this.#dedupe.delete(topic);
    await this.cmd({ cmd: "unsub", payload: { topic } });
  }

  /** Close for good — no reconnect. */
  close(): void {
    this.#closedByUser = true;
    this.#stopRefresh();
    if (this.#reconnectTimer !== null) this.#scheduler.clearTimeout(this.#reconnectTimer);
    this.#reconnectTimer = null;
    this.#ws?.close(1000, "client closing");
    this.#setState("closed");
  }

  // ── connection lifecycle ──────────────────────────────────────────────────

  #open(isReconnect: boolean): void {
    this.#setState(isReconnect ? "reconnecting" : "connecting");
    const ws = this.#socketFactory(this.#opts.url);
    this.#ws = ws;
    ws.onopen = () => void this.#onOpen();
    ws.onmessage = (ev) => this.#onMessage(ev.data);
    ws.onclose = (ev) => this.#onClose(ev.code);
    // `error` is advisory — a `close` always follows and drives recovery.
    ws.onerror = () => {};
  }

  async #onOpen(): Promise<void> {
    // First frame must be `auth` within Core's 3 s window (§4.1). On success
    // Core acks ok:true; on a bad token it closes 4401 (no ack) — the close
    // handler rejects the pending auth below, so this never hangs past the
    // ack timeout.
    const id = this.#nextId++;
    const acked = this.#pending.await(id);
    this.#rawSend({ id, cmd: "auth", payload: { token: this.#token } });
    try {
      await acked;
    } catch {
      // Auth failed (closed or errored). Recovery is the close handler's job;
      // force a close if the socket somehow stayed open.
      this.#ws?.close();
      return;
    }
    this.#setState("live");
    this.#startRefresh();
    void this.#resubscribeAll();
  }

  #onMessage(data: unknown): void {
    if (typeof data !== "string") return; // protocol is text-only
    let msg: unknown;
    try {
      msg = JSON.parse(data);
    } catch {
      return; // ignore unparseable frames rather than dying
    }
    if (typeof msg !== "object" || msg === null) return;
    if ("reply_to" in msg) {
      this.#pending.settle(msg as Ack);
      return;
    }
    if ("topic" in msg) {
      this.#onPush(msg as Push);
    }
  }

  #onPush(push: Push): void {
    // Dedupe + advance the resume watermark for channel messages only; every
    // other event is delivered as-is (they carry no per-message seq to dedupe).
    if (push.evt === "channels.message") {
      const ring = this.#ringFor(push.topic);
      if (!ring.admit(push.payload.message_id)) return; // duplicate replay → drop
      const sub = this.#subs.get(push.topic);
      if (sub) sub.lastSeq = Math.max(sub.lastSeq ?? 0, push.payload.seq);
    }
    const handlers = this.#topicHandlers.get(push.topic);
    if (handlers) for (const h of [...handlers]) h(push);
  }

  #onClose(code: number): void {
    this.#stopRefresh();
    this.#pending.rejectAll(new OpnError("closed", `connection closed (${code})`));
    this.#ws = null;
    if (this.#closedByUser) {
      this.#setState("closed");
      return;
    }
    switch (code) {
      case CLOSE.takenOver:
        // A newer session won. Terminal — surfacing this is the whole point of
        // the state (roadmap W0 exit: don't silently die).
        this.#setState("taken_over");
        return;
      case CLOSE.unauthorized:
        void this.#recoverAuth();
        return;
      case CLOSE.badFirstFrame:
        // We sent a non-auth first frame — a client bug, not a transient. Don't
        // reconnect into the same wall.
        this.#setState("closed");
        return;
      default:
        // 4409 slow-consumer, 1001 heartbeat, 1006 abnormal, network — all
        // transient. Back off and re-auth with the current token.
        this.#scheduleReconnect(false);
    }
  }

  /** A `4401` means the session is gone; only a fresh mint (remint) recovers it. */
  async #recoverAuth(): Promise<void> {
    if (!this.#opts.remint) {
      this.#setState("closed");
      return;
    }
    this.#setState("reconnecting");
    try {
      const token = await this.#opts.remint();
      this.#token = token;
      this.#opts.onToken?.(token);
      this.#scheduleReconnect(true);
    } catch {
      this.#setState("closed");
    }
  }

  #scheduleReconnect(immediate: boolean): void {
    if (this.#closedByUser) return;
    this.#setState("reconnecting");
    const delay = immediate ? 0 : backoffMs(this.#opts.maxBackoffMs ?? 3_000, this.#scheduler.random);
    this.#reconnectTimer = this.#scheduler.setTimeout(() => {
      this.#reconnectTimer = null;
      this.#open(true);
    }, delay);
  }

  async #resubscribeAll(): Promise<void> {
    const entries = [...this.#subs.entries()];
    // Fire concurrently; a single failed resub (membership changed) must not
    // block the rest. Its topic is dropped by `sub`'s catch on the retry path.
    await Promise.allSettled(
      entries.map(([topic, entry]) =>
        this.cmd({ cmd: "sub", payload: { topic, last_seq: entry.lastSeq } }),
      ),
    );
  }

  // ── auth.refresh timer ────────────────────────────────────────────────────

  #startRefresh(): void {
    this.#stopRefresh();
    const ttl = this.#opts.sessionTtlMs ?? 600_000;
    const skew = this.#opts.refreshSkewMs ?? 60_000;
    const delay = Math.max(1_000, ttl - skew);
    this.#refreshTimer = this.#scheduler.setTimeout(() => void this.#refresh(), delay);
  }

  #stopRefresh(): void {
    if (this.#refreshTimer !== null) this.#scheduler.clearTimeout(this.#refreshTimer);
    this.#refreshTimer = null;
  }

  async #refresh(): Promise<void> {
    try {
      const payload = await this.cmd({ cmd: "auth.refresh" });
      const token = (payload as { token?: unknown } | undefined)?.token;
      if (typeof token === "string") {
        this.#token = token;
        this.#opts.onToken?.(token);
      }
      this.#startRefresh(); // reschedule from the fresh expiry
    } catch {
      // Refresh failed (session revoked/expired underneath us). The live socket
      // survives until the next drop; recovery then runs via remint on 4401.
      // Don't tear down a working connection over a failed refresh.
    }
  }

  // ── helpers ───────────────────────────────────────────────────────────────

  #rawSend(frame: { id: number } & Cmd): void {
    this.#ws?.send(JSON.stringify(frame));
  }

  #ringFor(topic: string): DedupeRing {
    let ring = this.#dedupe.get(topic);
    if (!ring) {
      ring = new DedupeRing(this.#opts.dedupeRingSize);
      this.#dedupe.set(topic, ring);
    }
    return ring;
  }

  #setState(state: ConnectionState): void {
    if (this.#state === state) return;
    this.#state = state;
    for (const cb of [...this.#stateListeners]) cb(state);
  }
}
