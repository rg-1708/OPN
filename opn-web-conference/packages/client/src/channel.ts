import type { MessageBody, MessageItem, ReceiptKind } from "@opn/contracts";
import { OpnError } from "./errors.ts";
import type { OpnConnection } from "./connection.ts";
import type { Push } from "./types.ts";

/**
 * One message as the UI sees it. Keyed by `key` (the `client_uuid` while
 * optimistic, then the server `message_id`) so a render can reuse the same DOM
 * node across the sending → sent reconciliation and never double-render.
 */
export interface ChatMessage {
  /** Stable render key: `client_uuid` until acked, then `message_id`. */
  key: string;
  /** Present for messages this client sent (correlates the ack). */
  clientUuid?: string;
  /** Server id, once known (from the ack or the fan-out push). */
  messageId?: string;
  /** Server sequence, or `null` while still optimistic (renders at the tail). */
  seq: number | null;
  sender: string;
  body: MessageBody;
  /** RFC 3339 server timestamp, once known. */
  at: string | null;
  status: "sending" | "sent" | "failed";
  /** `sender === selfId` — whether this client authored it. */
  mine: boolean;
}

/** Per-member receipt watermarks (highest delivered/read seq). */
export interface Receipts {
  delivered: number;
  read: number;
}

export interface ChannelStoreOptions {
  /** The caller's own `character_id` — decides `mine` and is filtered from typing/receipts. */
  selfId: string;
  /** Called after any observable state change (messages, typing, receipts). */
  onChange: () => void;
  /** Min gap between `channels.typing` sends. Default 3000. */
  typingThrottleMs?: number;
  /** How long a `channels.typing` event marks a peer as typing. Default 5000. */
  typingTtlMs?: number;
  /** Debounce before auto `mark_delivered` after inbound messages. Default 400. */
  deliveredDebounceMs?: number;
  /**
   * Called when Core signals a resume gap too large to replay
   * (`channels.resume_overflow`): the store has dropped its acked state, so the
   * app must cold-load history over HTTP and `ingestHistory` it. Default no-op.
   */
  onColdLoad?: () => void;
}

/**
 * A framework-agnostic live-chat store over one `ch:<id>` topic (roadmap W1):
 * optimistic send with `client_uuid` idempotency, seq-ordered reconciliation,
 * resend-the-same-`client_uuid` on reconnect (the dedupe proof), typing,
 * watermark receipts, and HTTP-history merge. React-free and style-free — the
 * app renders from `messages()` in `onChange`.
 *
 * Ordering: acked messages sort by `seq`; still-sending ones sit at the tail in
 * send order. A message is deduped by `message_id`, so a fan-out push that
 * echoes our own send (Core publishes before it acks, so the push usually lands
 * first) reconciles against the optimistic entry instead of duplicating it.
 */
export class ChannelStore {
  readonly #conn: OpnConnection;
  readonly #channelId: string;
  readonly #topic: string;
  readonly #opts: Required<ChannelStoreOptions>;

  // Acked messages (have a seq), kept sorted by seq. Optimistic ones live in
  // #pending until their seq is known, then move here.
  readonly #acked: ChatMessage[] = [];
  readonly #pending: ChatMessage[] = [];
  readonly #byMessageId = new Map<string, ChatMessage>();
  readonly #byClientUuid = new Map<string, ChatMessage>();

  readonly #typing = new Map<string, number>(); // character_id → expires-at ms
  readonly #receipts = new Map<string, Receipts>(); // character_id → watermarks

  #lastSeqSeen = 0;
  #deliveredMarked = 0;
  #readMarked = 0;
  #lastTypingSent = 0;
  #deliveredTimer: ReturnType<typeof setTimeout> | null = null;
  readonly #typingTimers = new Set<ReturnType<typeof setTimeout>>();

  readonly #offPush: () => void;
  readonly #offState: () => void;

  constructor(conn: OpnConnection, channelId: string, opts: ChannelStoreOptions) {
    this.#conn = conn;
    this.#channelId = channelId;
    this.#topic = `ch:${channelId}`;
    this.#opts = {
      typingThrottleMs: 3_000,
      typingTtlMs: 5_000,
      deliveredDebounceMs: 400,
      onColdLoad: () => {},
      ...opts,
    };
    this.#offPush = conn.on(this.#topic, (push) => this.#onPush(push));
    // Resend still-unacked/failed sends the instant we're live again — same
    // client_uuid, so Core dedupes and no message renders twice (roadmap W1).
    this.#offState = conn.onState((s) => {
      if (s === "live") this.#resendUnacked();
    });
  }

  /** Subscribe the connection to this channel. Resolves once Core acks. */
  async subscribe(): Promise<void> {
    await this.#conn.sub(this.#topic);
  }

  /** Every message, ready to render: acked (by seq) then optimistic (send order). */
  messages(): ChatMessage[] {
    return [...this.#acked, ...this.#pending];
  }

  /** Lowest seq held, or `null` if empty — the `before_seq` cursor for loading older history. */
  oldestSeq(): number | null {
    return this.#acked[0]?.seq ?? null;
  }

  /** Peers currently typing (self excluded, expiry applied). */
  typingUsers(): string[] {
    const now = Date.now();
    const live: string[] = [];
    for (const [id, exp] of this.#typing) {
      if (exp > now) live.push(id);
      else this.#typing.delete(id);
    }
    return live;
  }

  /** Per-member delivered/read watermarks (self excluded). */
  receipts(): Map<string, Receipts> {
    return this.#receipts;
  }

  /**
   * Optimistically append a message and send it. Returns immediately; the entry
   * flips `sending → sent` on ack (or `failed` on a reject — a reconnect resends
   * it). Empty bodies are the app's problem; Core validates on the wire.
   */
  send(body: MessageBody): void {
    const clientUuid = globalThis.crypto.randomUUID();
    const msg: ChatMessage = {
      key: clientUuid,
      clientUuid,
      seq: null,
      sender: this.#opts.selfId,
      body,
      at: null,
      status: "sending",
      mine: true,
    };
    this.#pending.push(msg);
    this.#byClientUuid.set(clientUuid, msg);
    this.#opts.onChange();
    this.#dispatchSend(msg);
  }

  /**
   * Merge an HTTP history page (`GET /v1/channels/:id/messages`, newest-first)
   * into the store. Dedupes against live messages by `message_id`; safe to call
   * repeatedly for older pages.
   */
  ingestHistory(items: MessageItem[]): void {
    let changed = false;
    for (const it of items) {
      if (this.#byMessageId.has(it.message_id)) continue;
      const msg: ChatMessage = {
        key: it.message_id,
        messageId: it.message_id,
        seq: it.seq,
        sender: it.sender,
        body: it.body as MessageBody,
        at: it.at,
        status: "sent",
        mine: it.sender === this.#opts.selfId,
      };
      this.#insertAcked(msg);
      this.#lastSeqSeen = Math.max(this.#lastSeqSeen, it.seq);
      changed = true;
    }
    if (changed) this.#opts.onChange();
  }

  /** Throttled `channels.typing`. Call on each keystroke; it self-limits. */
  typing(): void {
    const now = Date.now();
    if (now - this.#lastTypingSent < this.#opts.typingThrottleMs) return;
    this.#lastTypingSent = now;
    void this.#conn
      .cmd({ cmd: "channels.typing", payload: { channel_id: this.#channelId } })
      .catch(() => {}); // typing is best-effort; a dropped one is nothing
  }

  /** Mark everything seen as read (call on focus). Idempotent — no-ops if nothing advanced. */
  markRead(): void {
    if (this.#lastSeqSeen <= this.#readMarked) return;
    this.#readMarked = this.#lastSeqSeen;
    this.#mark("channels.mark_read", this.#readMarked);
  }

  /** Detach handlers and cancel timers. The connection itself is left open. */
  dispose(): void {
    this.#offPush();
    this.#offState();
    if (this.#deliveredTimer) clearTimeout(this.#deliveredTimer);
    for (const t of this.#typingTimers) clearTimeout(t);
    this.#typingTimers.clear();
    void this.#conn.unsub(this.#topic).catch(() => {});
  }

  // ── internals ──────────────────────────────────────────────────────────────

  #dispatchSend(msg: ChatMessage): void {
    const clientUuid = msg.clientUuid!;
    this.#conn
      .cmd({
        cmd: "channels.send",
        payload: { channel_id: this.#channelId, client_uuid: clientUuid, body: msg.body },
      })
      .then((payload) => this.#onSendAck(msg, payload))
      .catch((err: unknown) => {
        // not_connected/closed/timeout → a reconnect resends. A real Core reject
        // (invalid/too_large) is terminal for this attempt.
        msg.status = "failed";
        const terminal = err instanceof OpnError && !["not_connected", "closed", "timeout"].includes(err.code);
        if (terminal) {
          this.#byClientUuid.delete(clientUuid);
        }
        this.#opts.onChange();
      });
  }

  #onSendAck(msg: ChatMessage, payload: unknown): void {
    const p = payload as { message_id?: string; seq?: number } | undefined;
    if (!p?.message_id || typeof p.seq !== "number") {
      msg.status = "failed";
      this.#opts.onChange();
      return;
    }
    const existing = this.#byMessageId.get(p.message_id);
    if (existing === msg) return; // already reconciled (the fan-out push adopted it)
    if (existing) {
      // A fan-out push already landed this message (the common case — Core
      // publishes before it acks). Drop the optimistic twin, keep the pushed
      // one, and stamp it as ours.
      this.#removePending(msg);
      this.#byClientUuid.delete(msg.clientUuid!);
      existing.mine = true;
      existing.clientUuid = msg.clientUuid;
      existing.status = "sent";
      this.#opts.onChange();
      return;
    }
    this.#promote(msg, p.message_id, p.seq, msg.at);
  }

  #onPush(push: Push): void {
    switch (push.evt) {
      case "channels.message":
        this.#onMessage(push.payload);
        break;
      case "channels.receipt":
        this.#onReceipt(push.payload);
        break;
      case "channels.typing":
        this.#onTyping(push.payload.character_id);
        break;
      case "channels.resume_overflow":
        this.#coldReload();
        break;
      // reactions/pins/members are the app's concern; ignore here.
    }
  }

  /**
   * Core signalled a resume gap too large to replay (`channels.resume_overflow`,
   * OPN-CORE.md §8). Incremental resume is void: drop all acked state and the
   * resume watermark, then ask the app to cold-load history over HTTP. Optimistic
   * (`#pending`) sends are kept — they resend by `client_uuid` and Core dedupes.
   */
  #coldReload(): void {
    this.#acked.length = 0;
    this.#byMessageId.clear();
    this.#receipts.clear();
    this.#typing.clear();
    this.#lastSeqSeen = 0;
    this.#deliveredMarked = 0;
    this.#readMarked = 0;
    this.#conn.resetTopic(this.#topic); // forget the stale watermark + dedupe ring
    this.#opts.onChange();
    this.#opts.onColdLoad();
  }

  #onMessage(p: {
    message_id: string;
    seq: number;
    sender: string;
    body: unknown;
    at: string;
  }): void {
    this.#lastSeqSeen = Math.max(this.#lastSeqSeen, p.seq);
    this.#scheduleDelivered();

    const existing = this.#byMessageId.get(p.message_id);
    if (existing) {
      // Already have it (our own acked send, or a resume replay the connection
      // let through). Fill in anything we were missing.
      if (existing.seq === null) existing.seq = p.seq;
      existing.at ??= p.at;
      return;
    }
    if (p.sender === this.#opts.selfId && this.#pending.length > 0) {
      // Our own send echoing back before its ack — adopt the oldest optimistic
      // entry (per-connection send order == fan-out order) instead of appending
      // a duplicate. Its ack lands later and no-ops.
      const mine = this.#pending[0]!;
      this.#promote(mine, p.message_id, p.seq, p.at);
      return;
    }
    const msg: ChatMessage = {
      key: p.message_id,
      messageId: p.message_id,
      seq: p.seq,
      sender: p.sender,
      body: p.body as MessageBody,
      at: p.at,
      status: "sent",
      mine: p.sender === this.#opts.selfId,
    };
    this.#insertAcked(msg);
    this.#opts.onChange();
  }

  #onReceipt(p: {
    character_id: string;
    kind: ReceiptKind;
    up_to_seq: number;
  }): void {
    if (p.character_id === this.#opts.selfId) return;
    const cur = this.#receipts.get(p.character_id) ?? { delivered: 0, read: 0 };
    if (p.kind === "delivered") cur.delivered = Math.max(cur.delivered, p.up_to_seq);
    else cur.read = Math.max(cur.read, p.up_to_seq);
    this.#receipts.set(p.character_id, cur);
    this.#opts.onChange();
  }

  #onTyping(characterId: string): void {
    if (characterId === this.#opts.selfId) return;
    this.#typing.set(characterId, Date.now() + this.#opts.typingTtlMs);
    this.#opts.onChange();
    const t = setTimeout(() => {
      this.#typingTimers.delete(t);
      this.#opts.onChange(); // let the app re-read typingUsers() and drop the expired one
    }, this.#opts.typingTtlMs + 50);
    this.#typingTimers.add(t);
  }

  /** Move an optimistic message into the acked list with its server seq. */
  #promote(msg: ChatMessage, messageId: string, seq: number, at: string | null): void {
    this.#removePending(msg);
    msg.messageId = messageId;
    msg.seq = seq;
    msg.at = at;
    msg.key = messageId;
    msg.status = "sent";
    this.#byMessageId.set(messageId, msg);
    if (msg.clientUuid) this.#byClientUuid.delete(msg.clientUuid);
    this.#insertAcked(msg);
    this.#lastSeqSeen = Math.max(this.#lastSeqSeen, seq);
    this.#opts.onChange();
  }

  #insertAcked(msg: ChatMessage): void {
    this.#byMessageId.set(msg.messageId!, msg);
    const seq = msg.seq!;
    // Newest at the end; binary-ish insert keeps the list seq-sorted cheaply.
    let i = this.#acked.length;
    while (i > 0 && (this.#acked[i - 1]!.seq ?? 0) > seq) i--;
    this.#acked.splice(i, 0, msg);
  }

  #removePending(msg: ChatMessage): void {
    const i = this.#pending.indexOf(msg);
    if (i >= 0) this.#pending.splice(i, 1);
  }

  #resendUnacked(): void {
    for (const msg of this.#pending) {
      if (msg.status === "failed" || msg.status === "sending") {
        msg.status = "sending";
        this.#dispatchSend(msg);
      }
    }
    if (this.#pending.length > 0) this.#opts.onChange();
  }

  #scheduleDelivered(): void {
    if (this.#deliveredTimer) return;
    this.#deliveredTimer = setTimeout(() => {
      this.#deliveredTimer = null;
      if (this.#lastSeqSeen > this.#deliveredMarked) {
        this.#deliveredMarked = this.#lastSeqSeen;
        this.#mark("channels.mark_delivered", this.#deliveredMarked);
      }
    }, this.#opts.deliveredDebounceMs);
  }

  #mark(cmd: "channels.mark_delivered" | "channels.mark_read", upToSeq: number): void {
    void this.#conn
      .cmd({ cmd, payload: { channel_id: this.#channelId, up_to_seq: upToSeq } })
      .catch(() => {}); // a lost watermark heals on the next one
  }
}

export function createChannelStore(
  conn: OpnConnection,
  channelId: string,
  opts: ChannelStoreOptions,
): ChannelStore {
  return new ChannelStore(conn, channelId, opts);
}
