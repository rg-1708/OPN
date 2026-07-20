// Test doubles for the wire runtime: a fake WebSocket the test drives frame by
// frame, and a virtual clock so backoff/refresh timers fire deterministically.
// No dependencies — runs under `node --test` with type-stripping.

/** A fake WS the test opens, feeds server frames, and closes at will. */
export class FakeSocket {
  readyState = 0;
  onopen: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  onclose: ((ev: { code: number; reason: string }) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;

  /** Every frame the client sent, parsed. */
  sent: Array<{ id?: number; cmd?: string; payload?: unknown }> = [];
  /** Set by the client's `close()` (does NOT auto-fire onclose — tests drive that). */
  closed: { code?: number; reason?: string } | null = null;
  /** Optional auto-responder: return a frame (or frames) to push back on each send. */
  autoRespond: ((frame: { id?: number; cmd?: string; payload?: unknown }) => unknown) | null = null;

  open(): void {
    this.readyState = 1;
    this.onopen?.({});
  }

  send(data: string): void {
    const frame = JSON.parse(data);
    this.sent.push(frame);
    const reply = this.autoRespond?.(frame);
    if (reply != null) {
      const frames = Array.isArray(reply) ? reply : [reply];
      for (const m of frames) queueMicrotask(() => this.onmessage?.({ data: JSON.stringify(m) }));
    }
  }

  /** Push one server frame to the client. */
  serverSend(obj: unknown): void {
    this.onmessage?.({ data: JSON.stringify(obj) });
  }

  /** Server-initiated close with a code. */
  serverClose(code: number, reason = ""): void {
    this.readyState = 3;
    this.onclose?.({ code, reason });
  }

  close(code?: number, reason?: string): void {
    this.closed = { code, reason };
    this.readyState = 3;
  }
}

/** Records every socket the factory hands out, newest last. */
export class SocketFactory {
  sockets: FakeSocket[] = [];
  /** Applied to each new socket before it is returned. */
  configure: ((s: FakeSocket) => void) | null = null;

  make = (_url: string): FakeSocket => {
    const s = new FakeSocket();
    this.configure?.(s);
    this.sockets.push(s);
    return s;
  };

  get last(): FakeSocket {
    const s = this.sockets.at(-1);
    if (!s) throw new Error("no socket created yet");
    return s;
  }
}

interface Timer {
  at: number;
  id: number;
  fn: () => void;
  cleared: boolean;
}

/** Virtual clock driving the injected scheduler. `advance` fires due timers in order. */
export class MockClock {
  now = 0;
  #timers: Timer[] = [];
  #id = 1;
  #rand = 0;

  setTimeout = (fn: () => void, ms: number): unknown => {
    const t: Timer = { at: this.now + ms, id: this.#id++, fn, cleared: false };
    this.#timers.push(t);
    return t.id;
  };

  clearTimeout = (handle: unknown): void => {
    const t = this.#timers.find((x) => x.id === handle);
    if (t) t.cleared = true;
  };

  random = (): number => this.#rand;

  setRandom(r: number): void {
    this.#rand = r;
  }

  /** Move the clock by `ms`, firing every timer that comes due (newly-scheduled ones too). */
  advance(ms: number): void {
    const target = this.now + ms;
    for (;;) {
      const due = this.#timers
        .filter((t) => !t.cleared && t.at <= target)
        .sort((a, b) => a.at - b.at || a.id - b.id);
      const next = due[0];
      if (!next) {
        this.now = target;
        return;
      }
      this.#timers = this.#timers.filter((x) => x !== next);
      this.now = next.at;
      next.fn();
    }
  }
}

/** Flush pending microtasks (lets the client's awaited promises settle). */
export function tick(): Promise<void> {
  return new Promise((resolve) => setImmediate(resolve));
}

/** The auth ack Core sends on a good first frame: `{ reply_to: id, ok: true }`. */
export function authOk(frame: { id?: number; cmd?: string }): unknown {
  if (frame.cmd === "auth") return { reply_to: frame.id, ok: true };
  return null;
}
