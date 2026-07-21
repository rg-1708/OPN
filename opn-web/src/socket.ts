import {
  OpnError,
  type AckPayload,
  type CmdName,
  type EvtName,
  type EvtPayload,
  type PayloadArgs,
} from "./types";

export type SocketState = "idle" | "connecting" | "open" | "closed";

/** Called on every (re)connect; return a fresh session JWT. */
export type TokenProvider = () => string | Promise<string>;

/** Server close codes (opn-core gateway/registry.rs). */
export const CLOSE = {
  BAD_FIRST_FRAME: 4400,
  UNAUTHORIZED: 4401,
  TAKEN_OVER: 4408,
  SLOW_CONSUMER: 4409,
} as const;

export interface OpnSocketOptions {
  /** e.g. `wss://core.example.com/ws` */
  url: string;
  /**
   * Session JWT, or a provider called on every (re)connect. Prefer a provider:
   * the JWT is short-lived, and a static string cannot survive a revoked
   * session or a long disconnect.
   */
  token: string | TokenProvider;
  /** Auto-reconnect on non-fatal closes. Default `true`. */
  reconnect?: boolean;
  /** Reconnect backoff bounds, exponential with jitter. Defaults 500 / 15000. */
  minBackoffMs?: number;
  maxBackoffMs?: number;
  /** Reject a pending ack after this long. Default 15000. */
  ackTimeoutMs?: number;
  /** Refresh the JWT this long before its `exp`. Default 60000. */
  refreshSkewMs?: number;
  /** Injectable WebSocket constructor for tests / non-browser runtimes. */
  webSocket?: new (url: string) => WebSocket;
}

interface Pending {
  resolve: (v: unknown) => void;
  reject: (e: OpnError) => void;
  timer: ReturnType<typeof setTimeout>;
}

interface Queued {
  name: string;
  payload: unknown;
  resolve: (v: unknown) => void;
  reject: (e: OpnError) => void;
}

interface SubEntry {
  refs: number;
  lastSeq: number | null;
}

function jwtExpMs(token: string): number | null {
  try {
    const b64 = token.split(".")[1]!.replace(/-/g, "+").replace(/_/g, "/");
    const exp = JSON.parse(atob(b64)).exp;
    return typeof exp === "number" ? exp * 1000 : null;
  } catch {
    return null;
  }
}

/**
 * The one multiplexed WSS connection to Core: auth-first-frame, id-correlated
 * acks as promises, topic subscriptions with `last_seq` resume, exponential
 * reconnect, and in-band JWT refresh. Framework-free; wire types come from
 * `@opn/contracts`.
 */
export class OpnSocket {
  private ws: WebSocket | null = null;
  private stateValue: SocketState = "idle";
  private nextId = 1;
  private pending = new Map<number, Pending>();
  private queue: Queued[] = [];
  private subs = new Map<string, SubEntry>();
  private listeners = new Map<string, Set<(...args: never[]) => void>>();
  private currentToken: string | null = null;
  private backoffMs: number;
  private manualClose = false;
  private authRetried = false;
  private refreshTimer: ReturnType<typeof setTimeout> | null = null;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;

  constructor(private readonly opts: OpnSocketOptions) {
    this.backoffMs = opts.minBackoffMs ?? 500;
  }

  get state(): SocketState {
    return this.stateValue;
  }

  /** The JWT currently in use — share it with `OpnHttp` for the read routes. */
  get token(): string | null {
    return this.currentToken;
  }

  async connect(): Promise<void> {
    if (this.stateValue === "connecting" || this.stateValue === "open") return;
    this.manualClose = false;
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
    this.reconnectTimer = null;
    this.setState("connecting");

    let token: string;
    try {
      token =
        typeof this.opts.token === "string"
          ? this.opts.token
          : await this.opts.token();
    } catch (e) {
      this.setState("closed");
      this.emit("error", e instanceof Error ? e : new Error(String(e)));
      this.scheduleReconnect();
      return;
    }
    this.currentToken = token;

    const WS = this.opts.webSocket ?? WebSocket;
    const ws = new WS(this.opts.url);
    this.ws = ws;
    ws.onmessage = (ev) => this.onMessage(ev.data);
    ws.onclose = (ev) => this.onClose(ws, ev.code, ev.reason);
    ws.onopen = () => {
      // First frame must be `auth`, within 3s of the upgrade (§4.1). A bad
      // token never acks — the server closes 4401 and onClose rejects it.
      this.request("auth", { token })
        .then(() => {
          this.backoffMs = this.opts.minBackoffMs ?? 500;
          this.authRetried = false;
          this.setState("open");
          this.scheduleRefresh();
          for (const [topic, s] of this.subs) {
            this.request("sub", { topic, last_seq: s.lastSeq }).catch((e) =>
              this.emit("error", e),
            );
          }
          const q = this.queue;
          this.queue = [];
          for (const item of q) {
            this.request(item.name, item.payload).then(item.resolve, item.reject);
          }
        })
        .catch(() => ws.close());
    };
  }

  /** Close for good: no reconnect, pending and queued commands reject. */
  close(): void {
    this.manualClose = true;
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
    this.reconnectTimer = null;
    if (this.refreshTimer) clearTimeout(this.refreshTimer);
    this.refreshTimer = null;
    const q = this.queue;
    this.queue = [];
    for (const item of q) item.reject(new OpnError("closed", "socket closed"));
    if (this.ws) this.ws.close(1000);
    else this.setState("closed");
  }

  /**
   * Send a command, resolve with its ack payload. Fully typed from the
   * contract: `cmd("channels.send", {...})`. While disconnected (and
   * reconnecting), commands queue and flush after re-auth.
   */
  cmd<N extends CmdName>(name: N, ...args: PayloadArgs<N>): Promise<AckPayload<N>> {
    const payload = args[0];
    if (this.stateValue === "open") {
      return this.request(name, payload) as Promise<AckPayload<N>>;
    }
    if (this.manualClose) {
      return Promise.reject(new OpnError("closed", "socket closed"));
    }
    return new Promise((resolve, reject) => {
      this.queue.push({ name, payload, resolve: resolve as (v: unknown) => void, reject });
    }) as Promise<AckPayload<N>>;
  }

  /**
   * Subscribe to a topic (see `topics` helpers). Reference-counted; the server
   * subscription is re-established on reconnect with the last seen `seq`, so
   * durable streams resume with replay (up to the server cap — watch for the
   * `channels.resume_overflow` event and cold-load history over HTTP).
   * Returns an unsubscribe function. Events arrive via `on()` / `onTopic()`.
   */
  sub(topic: string, lastSeq: number | null = null): () => void {
    let s = this.subs.get(topic);
    if (!s) {
      s = { refs: 0, lastSeq };
      this.subs.set(topic, s);
    }
    s.refs++;
    if (s.refs === 1 && this.stateValue === "open") {
      this.request("sub", { topic, last_seq: s.lastSeq }).catch((e) =>
        this.emit("error", e),
      );
    }
    let done = false;
    return () => {
      if (done) return;
      done = true;
      const cur = this.subs.get(topic);
      if (!cur) return;
      cur.refs--;
      if (cur.refs <= 0) {
        this.subs.delete(topic);
        if (this.stateValue === "open") {
          this.request("unsub", { topic }).catch(() => {});
        }
      }
    };
  }

  /** Listen for one event type across all subscribed topics. */
  on<N extends EvtName>(
    evt: N,
    fn: (payload: EvtPayload<N>, topic: string) => void,
  ): () => void {
    return this.addListener(`evt:${evt}`, fn as (...args: never[]) => void);
  }

  /** Listen for every event on one topic. */
  onTopic(
    topic: string,
    fn: (evt: EvtName, payload: unknown) => void,
  ): () => void {
    return this.addListener(`topic:${topic}`, fn as (...args: never[]) => void);
  }

  onState(fn: (state: SocketState) => void): () => void {
    return this.addListener("state", fn as (...args: never[]) => void);
  }

  /** Socket-level failures: sub re-auth errors, refresh loss, provider throws. */
  onError(fn: (err: Error) => void): () => void {
    return this.addListener("error", fn as (...args: never[]) => void);
  }

  /** Raw close notifications `(code, reason)` — see `CLOSE` for server codes. */
  onDisconnect(fn: (code: number, reason: string) => void): () => void {
    return this.addListener("close", fn as (...args: never[]) => void);
  }

  // ---- internals ----

  private request(name: string, payload?: unknown): Promise<unknown> {
    const id = this.nextId++;
    const frame =
      payload === undefined ? { id, cmd: name } : { id, cmd: name, payload };
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new OpnError("timeout", `${name}: no ack`));
      }, this.opts.ackTimeoutMs ?? 15_000);
      this.pending.set(id, { resolve, reject, timer });
      this.ws!.send(JSON.stringify(frame));
    });
  }

  private onMessage(data: unknown): void {
    let msg: any;
    try {
      msg = JSON.parse(String(data));
    } catch {
      return;
    }
    if (typeof msg.reply_to === "number") {
      const p = this.pending.get(msg.reply_to);
      if (!p) return;
      this.pending.delete(msg.reply_to);
      clearTimeout(p.timer);
      if (msg.ok) p.resolve(msg.payload);
      else
        p.reject(
          new OpnError(msg.err?.code ?? "internal", msg.err?.msg ?? "request failed"),
        );
      return;
    }
    if (typeof msg.topic === "string" && typeof msg.evt === "string") {
      const sub = this.subs.get(msg.topic);
      const seq = msg.payload?.seq;
      if (sub && typeof seq === "number" && (sub.lastSeq === null || seq > sub.lastSeq)) {
        sub.lastSeq = seq;
      }
      this.emit(`evt:${msg.evt}`, msg.payload, msg.topic);
      this.emit(`topic:${msg.topic}`, msg.evt, msg.payload);
    }
  }

  private onClose(ws: WebSocket, code: number, reason: string): void {
    if (ws !== this.ws) return; // stale socket from a previous attempt
    this.ws = null;
    if (this.refreshTimer) clearTimeout(this.refreshTimer);
    this.refreshTimer = null;
    for (const p of this.pending.values()) {
      clearTimeout(p.timer);
      p.reject(new OpnError("closed", reason || `socket closed (${code})`));
    }
    this.pending.clear();
    this.setState("closed");
    this.emit("close", code, reason);

    if (this.manualClose || this.opts.reconnect === false) return;
    if (code === CLOSE.TAKEN_OVER || code === CLOSE.BAD_FIRST_FRAME) {
      // Another connection owns this session / we sent garbage — reconnecting
      // would either fight the takeover or repeat the bug.
      this.emit("error", new OpnError("conflict", `not reconnecting (${code})`));
      return;
    }
    if (code === CLOSE.UNAUTHORIZED) {
      // Token rejected. If a provider can mint a fresh one, retry once.
      if (typeof this.opts.token === "string" || this.authRetried) {
        this.emit("error", new OpnError("unauthorized", "session token rejected"));
        return;
      }
      this.authRetried = true;
    }
    this.scheduleReconnect();
  }

  private scheduleReconnect(): void {
    if (this.manualClose || this.opts.reconnect === false || this.reconnectTimer) return;
    const jitter = 0.5 + Math.random() * 0.5;
    const delay = Math.round(this.backoffMs * jitter);
    this.backoffMs = Math.min(this.backoffMs * 2, this.opts.maxBackoffMs ?? 15_000);
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      void this.connect();
    }, delay);
  }

  private scheduleRefresh(): void {
    if (this.refreshTimer) clearTimeout(this.refreshTimer);
    this.refreshTimer = null;
    const expMs = this.currentToken ? jwtExpMs(this.currentToken) : null;
    if (expMs === null) return;
    const delay = Math.max(expMs - Date.now() - (this.opts.refreshSkewMs ?? 60_000), 5_000);
    this.refreshTimer = setTimeout(async () => {
      try {
        const ack = (await this.request("auth.refresh")) as { token: string };
        this.currentToken = ack.token;
        this.scheduleRefresh();
      } catch {
        // Session revoked/expired under us — drop the socket; the reconnect
        // path re-fetches a token from the provider.
        this.ws?.close();
      }
    }, delay);
  }

  private setState(s: SocketState): void {
    if (this.stateValue === s) return;
    this.stateValue = s;
    this.emit("state", s);
  }

  private addListener(key: string, fn: (...args: never[]) => void): () => void {
    let set = this.listeners.get(key);
    if (!set) {
      set = new Set();
      this.listeners.set(key, set);
    }
    set.add(fn);
    return () => set!.delete(fn);
  }

  private emit(key: string, ...args: unknown[]): void {
    const set = this.listeners.get(key);
    if (!set) return;
    for (const fn of [...set]) {
      try {
        (fn as (...a: unknown[]) => void)(...args);
      } catch (e) {
        // A listener throwing must not break dispatch for the rest.
        console.error("[opn] listener error", e);
      }
    }
  }
}
