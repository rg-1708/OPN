import assert from "node:assert/strict";
import { test } from "node:test";
import { connect, OpnError } from "../src/index.ts";
import type { ConnectionState } from "../src/index.ts";
import { authOk, FakeSocket, MockClock, SocketFactory, tick } from "./helpers.ts";

const ackAll = (f: { id?: number }) => ({ reply_to: f.id, ok: true });

interface Harness {
  conn: ReturnType<typeof connect>;
  factory: SocketFactory;
  clock: MockClock;
  states: ConnectionState[];
}

function build(opts: {
  respond?: FakeSocket["autoRespond"];
  remint?: () => Promise<string>;
  onToken?: (t: string) => void;
}): Harness {
  const clock = new MockClock();
  const factory = new SocketFactory();
  factory.configure = (s) => {
    s.autoRespond = opts.respond ?? null;
  };
  const states: ConnectionState[] = [];
  const conn = connect({
    url: "ws://test/ws",
    token: "T0",
    scheduler: clock,
    socketFactory: factory.make,
    remint: opts.remint,
    onToken: opts.onToken,
    sessionTtlMs: 600_000,
    refreshSkewMs: 60_000,
  });
  conn.onState((s) => states.push(s));
  return { conn, factory, clock, states };
}

/** Bring a fresh connection to `live`. */
async function goLive(h: Harness): Promise<void> {
  h.factory.last.open();
  await tick();
  await tick();
  assert.equal(h.conn.state, "live");
}

test("sends auth as the first frame and goes live on ok:true", async () => {
  const h = build({ respond: authOk });
  assert.equal(h.conn.state, "connecting");
  h.factory.last.open();
  await tick();
  const first = h.factory.last.sent[0];
  assert.equal(first?.cmd, "auth");
  assert.deepEqual(first?.payload, { token: "T0" });
  await tick();
  assert.equal(h.conn.state, "live");
});

test("auth never acked → times out via ackTimeout, never goes live", async () => {
  const h = build({ respond: null }); // Core never acks the first `auth` frame
  h.factory.last.open();
  await tick();
  assert.equal(h.factory.last.sent[0]?.cmd, "auth"); // auth sent, now awaiting the ack
  h.clock.advance(10_000); // past the default ackTimeoutMs
  await tick();
  assert.notEqual(h.conn.state, "live"); // timed out, not hung in live
});

test("bad token (4401) with no remint → closed, no reconnect", async () => {
  const h = build({ respond: null });
  h.factory.last.open();
  await tick(); // auth frame sent, awaiting ack
  h.factory.last.serverClose(4401);
  await tick();
  assert.equal(h.conn.state, "closed");
  assert.equal(h.factory.sockets.length, 1, "must not reconnect after unrecoverable auth");
});

test("takeover (4408) → taken_over, terminal", async () => {
  const h = build({ respond: authOk });
  await goLive(h);
  h.factory.last.serverClose(4408);
  await tick();
  assert.equal(h.conn.state, "taken_over");
  h.clock.advance(10_000);
  assert.equal(h.factory.sockets.length, 1, "taken_over must not reconnect");
});

test("bad first frame (4400) → closed, no reconnect", async () => {
  const h = build({ respond: authOk });
  await goLive(h);
  h.factory.last.serverClose(4400);
  await tick();
  assert.equal(h.conn.state, "closed");
  h.clock.advance(10_000);
  assert.equal(h.factory.sockets.length, 1);
});

test("ack correlation resolves interleaved acks by reply_to", async () => {
  const h = build({ respond: authOk });
  await goLive(h);
  const p2 = h.conn.cmd({ cmd: "identity.me" });
  const p3 = h.conn.cmd({ cmd: "channels.list" });
  const ids = h.factory.last.sent.filter((f) => f.cmd !== "auth").map((f) => f.id);
  assert.deepEqual(ids, [2, 3]);
  // Ack the second command first — order must not matter.
  h.factory.last.serverSend({ reply_to: 3, ok: true, payload: { which: 3 } });
  h.factory.last.serverSend({ reply_to: 2, ok: true, payload: { which: 2 } });
  assert.deepEqual(await p2, { which: 2 });
  assert.deepEqual(await p3, { which: 3 });
});

test("ok:false ack rejects with the Core error code", async () => {
  const h = build({ respond: authOk });
  await goLive(h);
  const p = h.conn.cmd({ cmd: "channels.list" });
  h.factory.last.serverSend({ reply_to: 2, ok: false, err: { code: "forbidden", msg: "nope" } });
  await assert.rejects(p, (e: unknown) => e instanceof OpnError && e.code === "forbidden");
});

test("cmd before live rejects not_connected", async () => {
  const h = build({ respond: authOk });
  await assert.rejects(
    h.conn.cmd({ cmd: "channels.list" }),
    (e: unknown) => e instanceof OpnError && e.code === "not_connected",
  );
});

test("auth.refresh timer re-mints the token and fires onToken", async () => {
  const tokens: string[] = [];
  const h = build({
    onToken: (t) => tokens.push(t),
    respond: (f) =>
      f.cmd === "auth.refresh"
        ? { reply_to: f.id, ok: true, payload: { token: "T1" } }
        : { reply_to: f.id, ok: true },
  });
  await goLive(h);
  h.clock.advance(600_000 - 60_000); // refresh fires at ttl - skew
  await tick();
  await tick();
  const refreshed = h.factory.last.sent.find((f) => f.cmd === "auth.refresh");
  assert.ok(refreshed, "auth.refresh was sent");
  assert.equal(h.conn.token, "T1");
  assert.deepEqual(tokens, ["T1"]);
});

test("transient close reconnects and resubscribes with last_seq", async () => {
  const h = build({ respond: ackAll });
  await goLive(h);
  await h.conn.sub("ch:room");
  // A live message advances the resume watermark to seq 7.
  h.factory.last.serverSend({
    topic: "ch:room",
    evt: "channels.message",
    payload: { channel_id: "room", message_id: "m7", seq: 7, sender: "s", body: {}, at: "t" },
  });
  const before = h.factory.sockets.length;
  h.factory.last.serverClose(4409); // slow-consumer → transient
  assert.equal(h.conn.state, "reconnecting");
  h.clock.advance(1); // fire the (0-jitter) reconnect timer
  assert.equal(h.factory.sockets.length, before + 1, "opened a fresh socket");
  await goLive(h);
  const resub = h.factory.last.sent.find((f) => f.cmd === "sub");
  assert.ok(resub, "resubscribed on reconnect");
  assert.deepEqual(resub?.payload, { topic: "ch:room", last_seq: 7 });
});

test("duplicate message_id is delivered to the app exactly once", async () => {
  const h = build({ respond: ackAll });
  await goLive(h);
  await h.conn.sub("ch:room");
  const got: string[] = [];
  h.conn.on("ch:room", (push) => {
    if (push.evt === "channels.message") got.push(push.payload.message_id);
  });
  const msg = {
    topic: "ch:room",
    evt: "channels.message",
    payload: { channel_id: "room", message_id: "dup", seq: 3, sender: "s", body: {}, at: "t" },
  };
  h.factory.last.serverSend(msg); // live
  h.factory.last.serverSend(msg); // replayed on resume
  assert.deepEqual(got, ["dup"]);
});

test("remint recovers a 4401 and reconnects with the fresh token", async () => {
  const tokens: string[] = [];
  const h = build({
    respond: ackAll,
    remint: async () => "REMINT",
    onToken: (t) => tokens.push(t),
  });
  await goLive(h);
  h.factory.last.serverClose(4401);
  await tick();
  await tick(); // remint() settles
  assert.deepEqual(tokens, ["REMINT"]);
  h.clock.advance(1); // fire the immediate reconnect
  await goLive(h);
  const auth = h.factory.last.sent.find((f) => f.cmd === "auth");
  assert.deepEqual(auth?.payload, { token: "REMINT" });
});
