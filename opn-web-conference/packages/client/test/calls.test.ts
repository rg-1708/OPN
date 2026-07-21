import assert from "node:assert/strict";
import { test } from "node:test";
import { connect, createCallManager } from "../src/index.ts";
import type { CallManager } from "../src/index.ts";
import { MockClock, SocketFactory, tick } from "./helpers.ts";

const SELF = "me";
const PEER = "them";

type Frame = { id?: number; cmd?: string; payload?: any };

/** A peer connection with just the surface PeerLink drives. */
class FakePeer {
  onicecandidate: ((ev: { candidate: unknown }) => void) | null = null;
  ontrack: ((ev: { streams: unknown[]; track: unknown }) => void) | null = null;
  onconnectionstatechange: (() => void) | null = null;
  connectionState = "new";
  localDescription: { type: string; sdp?: string } | null = null;
  remoteDescription: { type: string; sdp?: string } | null = null;
  ice: unknown[] = [];
  addTrack(): void {}
  createOffer(): Promise<{ type: string; sdp: string }> {
    return Promise.resolve({ type: "offer", sdp: "OFFER_SDP" });
  }
  createAnswer(): Promise<{ type: string; sdp: string }> {
    return Promise.resolve({ type: "answer", sdp: "ANSWER_SDP" });
  }
  setLocalDescription(d: { type: string; sdp?: string }): Promise<void> {
    this.localDescription = d;
    return Promise.resolve();
  }
  setRemoteDescription(d: { type: string; sdp?: string }): Promise<void> {
    this.remoteDescription = d;
    return Promise.resolve();
  }
  addIceCandidate(c: unknown): Promise<void> {
    this.ice.push(c);
    return Promise.resolve();
  }
  close(): void {
    this.connectionState = "closed";
  }
}

function fakeStream(): MediaStream {
  return { getTracks: () => [{ stop() {} }] } as unknown as MediaStream;
}

const active = (callId: string, kind: string) => ({
  topic: `call:${callId}`,
  evt: "calls.state",
  payload: {
    call_id: callId,
    kind,
    state: "active",
    participants: [
      { character_id: SELF, state: "joined" },
      { character_id: PEER, state: "joined" },
    ],
    ice_servers: [],
  },
});

const ended = (callId: string, kind: string) => ({
  topic: `call:${callId}`,
  evt: "calls.state",
  payload: {
    call_id: callId,
    kind,
    state: "ended",
    participants: [
      { character_id: SELF, state: "left" },
      { character_id: PEER, state: "left" },
    ],
    ice_servers: [],
  },
});

async function build(
  startOverride?: (f: Frame) => unknown,
  getMedia?: () => Promise<MediaStream>,
): Promise<{
  mgr: CallManager;
  peers: FakePeer[];
  sent: () => Frame[];
  serverSend: (obj: unknown) => void;
}> {
  const clock = new MockClock();
  const factory = new SocketFactory();
  const responder = (f: Frame): unknown => {
    if (f.cmd === "auth") return { reply_to: f.id, ok: true };
    if (f.cmd === "calls.start") {
      return startOverride
        ? startOverride(f)
        : { reply_to: f.id, ok: true, payload: { call_id: "call1" } };
    }
    return { reply_to: f.id, ok: true };
  };
  factory.configure = (s) => (s.autoRespond = responder);
  const conn = connect({ url: "ws://t/ws", token: "T", scheduler: clock, socketFactory: factory.make });
  factory.last.open();
  await tick();
  await tick();
  assert.equal(conn.state, "live");

  const peers: FakePeer[] = [];
  const mgr = createCallManager(conn, {
    selfId: SELF,
    onChange: () => {},
    getMedia: getMedia ?? (() => Promise.resolve(fakeStream())),
    peerFactory: () => {
      const p = new FakePeer();
      peers.push(p);
      return p as unknown as RTCPeerConnection;
    },
    defaultIceServers: [{ urls: "stun:test" }],
  });
  return {
    mgr,
    peers,
    sent: () => factory.last.sent,
    serverSend: (obj) => factory.last.serverSend(obj),
  };
}

/** Let the enter-call chain (getMedia → offer/answer → signal) settle. */
async function settle(): Promise<void> {
  for (let i = 0; i < 4; i++) await tick();
}

test("caller: active snapshot builds the offer, answer sets remote, ended terminates", async () => {
  const h = await build();
  await h.mgr.start("5550001", true);
  assert.equal(h.mgr.view().phase, "calling");
  assert.equal(h.mgr.view().callId, "call1");
  assert.ok(
    h.sent().some((f) => f.cmd === "sub" && f.payload.topic === "call:call1"),
    "subscribed to the call topic",
  );

  h.serverSend(active("call1", "video"));
  await settle();
  assert.equal(h.mgr.view().phase, "active");
  assert.equal(h.mgr.view().peer, PEER);

  const offer = h.sent().find((f) => f.cmd === "calls.signal" && f.payload.payload?.kind === "offer");
  assert.ok(offer, "caller sent a WebRTC offer");
  assert.equal(offer!.payload.to, PEER);
  assert.equal(offer!.payload.call_id, "call1");

  h.serverSend({
    topic: "call:call1",
    evt: "calls.signal",
    payload: { call_id: "call1", from: PEER, to: SELF, payload: { kind: "answer", sdp: "A" } },
  });
  await tick();
  assert.equal(h.peers[0]!.remoteDescription?.type, "answer");

  h.serverSend({
    topic: "call:call1",
    evt: "calls.state",
    payload: {
      call_id: "call1",
      kind: "video",
      state: "ended",
      participants: [
        { character_id: SELF, state: "left" },
        { character_id: PEER, state: "left" },
      ],
      ice_servers: [],
    },
  });
  await tick();
  assert.equal(h.mgr.view().phase, "ended");
  assert.equal(h.mgr.view().endReason, "ended");
});

test("caller: a busy (conflict) ack ends the call with reason 'busy'", async () => {
  const h = await build((f) => ({ reply_to: f.id, ok: false, err: { code: "conflict", msg: "busy" } }));
  await h.mgr.start("5550002", false);
  assert.equal(h.mgr.view().phase, "ended");
  assert.equal(h.mgr.view().endReason, "busy");
});

test("callee: ring → accept answers the offer, sends no offer of its own", async () => {
  const h = await build();
  h.mgr.onRing("call2");
  await tick();
  assert.equal(h.mgr.view().phase, "ringing");
  assert.ok(h.sent().some((f) => f.cmd === "sub" && f.payload.topic === "call:call2"));

  await h.mgr.accept();
  assert.ok(h.sent().some((f) => f.cmd === "calls.accept" && f.payload.call_id === "call2"));

  h.serverSend(active("call2", "voice"));
  await settle();
  assert.equal(h.mgr.view().phase, "active");
  assert.ok(
    !h.sent().some((f) => f.cmd === "calls.signal" && f.payload.payload?.kind === "offer"),
    "callee is the answerer — never offers",
  );

  h.serverSend({
    topic: "call:call2",
    evt: "calls.signal",
    payload: { call_id: "call2", from: PEER, to: SELF, payload: { kind: "offer", sdp: "O" } },
  });
  await settle();
  const answer = h.sent().find((f) => f.cmd === "calls.signal" && f.payload.payload?.kind === "answer");
  assert.ok(answer, "callee answered the incoming offer");
  assert.equal(answer!.payload.to, PEER);
});

test("call ending during getUserMedia stops the tracks and builds no link (no media leak)", async () => {
  let resolveMedia!: (s: MediaStream) => void;
  const stopped: number[] = [];
  const heldStream = { getTracks: () => [{ stop: () => stopped.push(1) }] } as unknown as MediaStream;
  const h = await build(undefined, () => new Promise<MediaStream>((r) => (resolveMedia = r)));

  await h.mgr.start("5550009", true);
  h.serverSend(active("call1", "video")); // → phase active → #enterCall awaits getUserMedia
  await tick();
  assert.equal(h.mgr.view().phase, "active");

  // The peer hangs up (or ring times out) while our media is still resolving.
  h.serverSend(ended("call1", "video"));
  await tick();
  assert.equal(h.mgr.view().phase, "ended");

  // Media finally resolves: the guard must stop the tracks and not wire up a call.
  resolveMedia(heldStream);
  await settle();
  assert.deepEqual(stopped, [1], "acquired media tracks were stopped");
  assert.equal(h.peers.length, 0, "no PeerLink built after the call ended");
  assert.ok(!h.sent().some((f) => f.cmd === "calls.signal"), "no WebRTC signal on a dead call");
});

test("a stale 'ended' view does not block the next incoming ring", async () => {
  const h = await build();
  await h.mgr.start("5550010", false);
  h.serverSend(ended("call1", "voice"));
  await tick();
  assert.equal(h.mgr.view().phase, "ended");

  // A fresh ring arrives before the user dismisses the ended card — must ring, not auto-decline.
  h.mgr.onRing("call2");
  await tick();
  assert.equal(h.mgr.view().phase, "ringing");
  assert.equal(h.mgr.view().callId, "call2");
  assert.ok(
    !h.sent().some((f) => f.cmd === "calls.decline" && f.payload.call_id === "call2"),
    "the new ring was not swallowed by the stale ended view",
  );
});

test("dialing out after a stale 'ended' view starts a fresh call", async () => {
  const h = await build();
  await h.mgr.start("5550011", false);
  h.serverSend(ended("call1", "voice"));
  await tick();
  assert.equal(h.mgr.view().phase, "ended");

  await h.mgr.start("5550012", true);
  assert.equal(h.mgr.view().phase, "calling", "the call-back button works without a manual Close");
  assert.equal(h.mgr.view().isCaller, true);
});

test("callee: decline sends calls.decline and ends", async () => {
  const h = await build();
  h.mgr.onRing("call3");
  await tick();
  h.mgr.decline();
  assert.ok(h.sent().some((f) => f.cmd === "calls.decline" && f.payload.call_id === "call3"));
  assert.equal(h.mgr.view().phase, "ended");
  assert.equal(h.mgr.view().endReason, "declined");
});
