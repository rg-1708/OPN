import assert from "node:assert/strict";
import { test } from "node:test";
import { connect, createChannelStore } from "../src/index.ts";
import type { ChannelStore } from "../src/index.ts";
import { FakeSocket, MockClock, SocketFactory, tick } from "./helpers.ts";

const CH = "room";
const SELF = "me";

type Frame = { id?: number; cmd?: string; payload?: unknown };
type Responder = (f: Frame) => unknown;

/** Read `client_uuid` off a captured send frame. */
const clientUuid = (f: Frame): string | undefined =>
  (f.payload as { client_uuid?: string } | undefined)?.client_uuid;

interface Harness {
  store: ChannelStore;
  conn: ReturnType<typeof connect>;
  factory: SocketFactory;
  clock: MockClock;
  changes: number;
  sends: () => Frame[];
}

/** Default: ack auth/sub/marks; assign a fresh seq per channels.send. */
function defaultResponder(): Responder {
  let seq = 0;
  return (f) => {
    if (f.cmd === "auth") return { reply_to: f.id, ok: true };
    if (f.cmd === "channels.send") {
      seq += 1;
      return { reply_to: f.id, ok: true, payload: { message_id: `m${seq}`, seq } };
    }
    return { reply_to: f.id, ok: true };
  };
}

async function build(responder: Responder = defaultResponder()): Promise<Harness> {
  const clock = new MockClock();
  const factory = new SocketFactory();
  factory.configure = (s) => {
    s.autoRespond = responder;
  };
  const conn = connect({
    url: "ws://test/ws",
    token: "T0",
    scheduler: clock,
    socketFactory: factory.make,
  });
  factory.last.open();
  await tick();
  await tick();
  assert.equal(conn.state, "live");
  const h: Harness = {
    conn,
    factory,
    clock,
    changes: 0,
    store: null as unknown as ChannelStore,
    sends: () => factory.last.sent.filter((f) => f.cmd === "channels.send"),
  };
  h.store = createChannelStore(conn, CH, { selfId: SELF, onChange: () => (h.changes += 1) });
  return h;
}

/** A `channels.message` push on the channel topic. */
function push(sock: FakeSocket, p: Partial<Record<string, unknown>>): void {
  sock.serverSend({
    topic: `ch:${CH}`,
    evt: "channels.message",
    payload: {
      channel_id: CH,
      message_id: "x",
      seq: 1,
      sender: "other",
      body: { text: "hi" },
      at: "2026-01-01T00:00:00Z",
      ...p,
    },
  });
}

test("optimistic send: appends immediately, reconciles to sent on ack", async () => {
  const h = await build();
  h.store.send({ text: "hello", media_ids: null, gif_url: null, meta: null });
  let msgs = h.store.messages();
  assert.equal(msgs.length, 1);
  assert.equal(msgs[0]!.status, "sending");
  assert.equal(msgs[0]!.seq, null);
  assert.ok(msgs[0]!.mine);
  await tick();
  await tick();
  msgs = h.store.messages();
  assert.equal(msgs.length, 1, "no duplicate after ack");
  assert.equal(msgs[0]!.status, "sent");
  assert.equal(msgs[0]!.seq, 1);
  assert.equal(msgs[0]!.messageId, "m1");
});

test("push-before-ack: fan-out echo of our own send does not duplicate", async () => {
  // Core publishes before it acks, so the push usually lands first.
  const responder: Responder = (f) => {
    if (f.cmd === "auth") return { reply_to: f.id, ok: true };
    if (f.cmd === "channels.send") {
      const echo = {
        topic: `ch:${CH}`,
        evt: "channels.message",
        payload: {
          channel_id: CH,
          message_id: "m1",
          seq: 9,
          sender: SELF,
          body: {},
          at: "t",
        },
      };
      return [echo, { reply_to: f.id, ok: true, payload: { message_id: "m1", seq: 9 } }];
    }
    return { reply_to: f.id, ok: true };
  };
  const h = await build(responder);
  h.store.send({ text: "hi", media_ids: null, gif_url: null, meta: null });
  await tick();
  await tick();
  const msgs = h.store.messages();
  assert.equal(msgs.length, 1, "optimistic + echoed push collapse to one");
  assert.equal(msgs[0]!.seq, 9);
  assert.equal(msgs[0]!.messageId, "m1");
  assert.ok(msgs[0]!.mine, "still marked ours");
  assert.equal(msgs[0]!.status, "sent");
});

test("remote messages append and stay seq-ordered regardless of arrival order", async () => {
  const h = await build();
  push(h.factory.last, { message_id: "b", seq: 5, sender: "other" });
  push(h.factory.last, { message_id: "a", seq: 3, sender: "other" });
  const seqs = h.store.messages().map((m) => m.seq);
  assert.deepEqual(seqs, [3, 5]);
  assert.equal(h.store.messages()[0]!.mine, false);
});

test("history merge dedupes against a live message by message_id", async () => {
  const h = await build();
  push(h.factory.last, { message_id: "dup", seq: 4, sender: "other" });
  h.store.ingestHistory([
    { message_id: "old", seq: 2, sender: "other", body: {}, at: "t" },
    { message_id: "dup", seq: 4, sender: "other", body: {}, at: "t" },
  ]);
  const ids = h.store.messages().map((m) => m.messageId);
  assert.deepEqual(ids, ["old", "dup"], "no duplicate 'dup', ordered by seq");
});

test("resend on reconnect reuses the same client_uuid (dedupe proof)", async () => {
  const h = await build();
  h.store.send({ text: "retry me", media_ids: null, gif_url: null, meta: null });
  const firstUuid = clientUuid(h.sends()[0]!);
  assert.ok(firstUuid);
  // Drop before the ack: the in-flight send rejects (closed) → status failed.
  h.factory.last.serverClose(4409);
  await tick();
  assert.equal(h.store.messages()[0]!.status, "failed");
  // Reconnect → back to live → the store resends with the same client_uuid.
  h.clock.advance(1);
  h.factory.last.open();
  await tick();
  await tick();
  assert.equal(h.conn.state, "live");
  const resent = h.sends();
  assert.equal(resent.length, 1, "one resend on the fresh socket");
  assert.equal(clientUuid(resent[0]!), firstUuid, "same client_uuid");
});

test("receipts and typing surface per-peer, self filtered out", async () => {
  const h = await build();
  const sock = h.factory.last;
  sock.serverSend({
    topic: `ch:${CH}`,
    evt: "channels.receipt",
    payload: { channel_id: CH, character_id: "other", kind: "read", up_to_seq: 7, at: "t" },
  });
  sock.serverSend({
    topic: `ch:${CH}`,
    evt: "channels.receipt",
    payload: { channel_id: CH, character_id: SELF, kind: "read", up_to_seq: 9, at: "t" },
  });
  assert.equal(h.store.receipts().get("other")?.read, 7);
  assert.equal(h.store.receipts().has(SELF), false, "own receipt ignored");

  sock.serverSend({
    topic: `ch:${CH}`,
    evt: "channels.typing",
    payload: { channel_id: CH, character_id: "other" },
  });
  assert.deepEqual(h.store.typingUsers(), ["other"]);
});
