import type { CallKind, CallParticipant, CallSessionState } from "@opn/contracts";
import { OpnError } from "./errors.ts";
import type { OpnConnection } from "./connection.ts";
import type { Push } from "./types.ts";

/**
 * 1:1 WebRTC calls over the OPN data plane (roadmap W2). Framework-agnostic and
 * style-free: the app renders from `view()` in `onChange`, and this same module
 * is the signaling code both templates share (only track acquisition differs —
 * browser sends an audio track too, FiveM keeps audio in pma-voice, OPN.md §6).
 *
 * Wire model (opn-core §10.4):
 *  - Caller `calls.start { callee_number, video }` → ack carries `call_id`
 *    (dialer needs no standing sub); then `sub call:<id>`.
 *  - Callee gets a `notify` class `ring` carrying `call_id`; app calls `onRing`,
 *    which subs `call:<id>` (participants-only) and shows the incoming state.
 *  - `calls.state` = full snapshot on `call:<id>` every change; carries
 *    `ice_servers`. Both sides set up the peer connection on the first `active`
 *    snapshot (both are subbed by then, and ICE config is known).
 *  - `calls.signal { call_id, to, payload }` relays our opaque offer/answer/ICE
 *    envelope to the peer's sessions as a `calls.signal` event on `call:<id>`.
 */

/** Our opaque WebRTC envelope, carried in the `calls.signal` payload. */
export type SignalMsg =
  | { kind: "offer"; sdp: string }
  | { kind: "answer"; sdp: string }
  | { kind: "ice"; candidate: RTCIceCandidateInit };

/** Call lifecycle as the UI sees it (a flattened view of the Core FSM + local media). */
export type CallPhase =
  | "idle" // no call
  | "calling" // outgoing, ringing the callee
  | "ringing" // incoming, not yet accepted
  | "active" // media flowing
  | "ended"; // terminal — call `clear()` to return to idle

/** Why a call left `active`/`ringing` — surfaced as distinct UI, not a console log. */
export type CallEndReason = "busy" | "declined" | "no_answer" | "ended" | "failed";

export interface CallView {
  phase: CallPhase;
  callId: string | null;
  kind: CallKind;
  /** True for the side that dialed. */
  isCaller: boolean;
  /** The other participant's `character_id`, once a snapshot names them. */
  peer: string | null;
  localStream: MediaStream | null;
  remoteStream: MediaStream | null;
  /** Set when `phase === "ended"`. */
  endReason: CallEndReason | null;
}

export interface CallManagerOptions {
  /** Own `character_id` — picks "the other participant" out of every snapshot. */
  selfId: string;
  /** Called after any observable change (phase, streams, peer). */
  onChange: (view: CallView) => void;
  /** Acquire local media for a call kind. Default: mic + capped ~480p camera. Tests inject a fake. */
  getMedia?: (kind: CallKind) => Promise<MediaStream>;
  /** Build a peer connection. Default: `new RTCPeerConnection(config)`. Tests inject a fake. */
  peerFactory?: (config: RTCConfiguration) => RTCPeerConnection;
  /** ICE servers used when a snapshot carries none. Default: a public STUN. */
  defaultIceServers?: RTCIceServer[];
}

/** ~480p, mic on — matches the FiveM video budget (roadmap W2). */
function defaultGetMedia(kind: CallKind): Promise<MediaStream> {
  return navigator.mediaDevices.getUserMedia({
    audio: true,
    video:
      kind === "video"
        ? { width: { ideal: 640 }, height: { ideal: 480 }, frameRate: { max: 30 } }
        : false,
  });
}

const PUBLIC_STUN: RTCIceServer[] = [{ urls: "stun:stun.l.google.com:19302" }];

/**
 * One call's WebRTC peer connection. The caller is the offerer; the callee
 * answers. Remote ICE candidates that arrive before the remote description is
 * set are buffered and flushed — the two SDP directions can interleave on the
 * wire, so this ordering guard is load-bearing, not defensive dressing.
 */
class PeerLink {
  readonly #pc: RTCPeerConnection;
  readonly #send: (m: SignalMsg) => void;
  readonly #isOfferer: boolean;
  readonly #onError: () => void;
  #remoteSet = false;
  readonly #pendingIce: RTCIceCandidateInit[] = [];

  constructor(opts: {
    pc: RTCPeerConnection;
    localStream: MediaStream;
    isOfferer: boolean;
    send: (m: SignalMsg) => void;
    onRemote: (stream: MediaStream) => void;
    onError: () => void;
  }) {
    this.#pc = opts.pc;
    this.#send = opts.send;
    this.#isOfferer = opts.isOfferer;
    this.#onError = opts.onError;
    this.#pc.onicecandidate = (ev) => {
      if (ev.candidate) this.#send({ kind: "ice", candidate: ev.candidate.toJSON() });
    };
    this.#pc.ontrack = (ev) => {
      opts.onRemote(ev.streams[0] ?? new MediaStream([ev.track]));
    };
    this.#pc.onconnectionstatechange = () => {
      if (this.#pc.connectionState === "failed") this.#onError();
    };
    for (const track of opts.localStream.getTracks()) this.#pc.addTrack(track, opts.localStream);
  }

  /** Offerer only: create and send the initial offer. */
  async start(): Promise<void> {
    if (!this.#isOfferer) return;
    try {
      const offer = await this.#pc.createOffer();
      await this.#pc.setLocalDescription(offer);
      this.#send({ kind: "offer", sdp: offer.sdp! });
    } catch {
      this.#onError();
    }
  }

  async handleSignal(m: SignalMsg): Promise<void> {
    try {
      if (m.kind === "offer") {
        await this.#pc.setRemoteDescription({ type: "offer", sdp: m.sdp });
        this.#markRemote();
        const answer = await this.#pc.createAnswer();
        await this.#pc.setLocalDescription(answer);
        this.#send({ kind: "answer", sdp: answer.sdp! });
      } else if (m.kind === "answer") {
        await this.#pc.setRemoteDescription({ type: "answer", sdp: m.sdp });
        this.#markRemote();
      } else if (this.#remoteSet) {
        await this.#pc.addIceCandidate(m.candidate);
      } else {
        this.#pendingIce.push(m.candidate); // no remote description yet — buffer
      }
    } catch {
      this.#onError();
    }
  }

  #markRemote(): void {
    this.#remoteSet = true;
    for (const c of this.#pendingIce) void this.#pc.addIceCandidate(c).catch(() => {});
    this.#pendingIce.length = 0;
  }

  close(): void {
    this.#pc.onicecandidate = null;
    this.#pc.ontrack = null;
    this.#pc.onconnectionstatechange = null;
    try {
      this.#pc.close();
    } catch {
      /* already closed */
    }
  }
}

/**
 * Drives one call at a time (Core calls are strictly 1:1; a second ring while
 * busy is auto-declined). Owns the `call:<id>` subscription, mirrors the Core
 * FSM from `calls.state` snapshots, and bridges WebRTC signaling through
 * `calls.signal`. Never guesses call state client-side — every phase change is
 * a snapshot (opn-core §10.4 "kills delta desync").
 */
export class CallManager {
  readonly #conn: OpnConnection;
  readonly #selfId: string;
  readonly #onChange: (view: CallView) => void;
  readonly #getMedia: (kind: CallKind) => Promise<MediaStream>;
  readonly #peerFactory: (config: RTCConfiguration) => RTCPeerConnection;
  readonly #defaultIce: RTCIceServer[];

  #view: CallView = CallManager.#idle();
  #offCall: (() => void) | null = null;
  #link: PeerLink | null = null;
  #entering = false; // guards double `#enterCall` across async snapshots
  #wasActive = false;
  readonly #pendingSignals: SignalMsg[] = []; // signals that arrived before the link existed

  constructor(conn: OpnConnection, opts: CallManagerOptions) {
    this.#conn = conn;
    this.#selfId = opts.selfId;
    this.#onChange = opts.onChange;
    this.#getMedia = opts.getMedia ?? defaultGetMedia;
    this.#peerFactory = opts.peerFactory ?? ((config) => new RTCPeerConnection(config));
    this.#defaultIce = opts.defaultIceServers ?? PUBLIC_STUN;
  }

  static #idle(): CallView {
    return {
      phase: "idle",
      callId: null,
      kind: "voice",
      isCaller: false,
      peer: null,
      localStream: null,
      remoteStream: null,
      endReason: null,
    };
  }

  view(): CallView {
    return this.#view;
  }

  /** Dial `calleeNumber`. No-op if a call is already in flight (Core enforces 1:1 too). */
  async start(calleeNumber: string, video: boolean): Promise<void> {
    if (this.#view.phase === "ended") this.#view = CallManager.#idle(); // clear a stale ended card
    if (this.#view.phase !== "idle") return;
    this.#view = {
      ...CallManager.#idle(),
      phase: "calling",
      kind: video ? "video" : "voice",
      isCaller: true,
    };
    this.#onChange(this.#view);
    try {
      const ack = (await this.#conn.cmd({
        cmd: "calls.start",
        payload: { callee_number: calleeNumber, video },
      })) as { call_id?: string } | undefined;
      const callId = ack?.call_id;
      if (typeof callId !== "string") throw new OpnError("internal", "calls.start returned no call_id");
      this.#view.callId = callId;
      await this.#watch(callId);
    } catch (err) {
      // `conflict` is the callee-busy signal; everything else is a start failure.
      const reason: CallEndReason =
        err instanceof OpnError && err.code === "conflict" ? "busy" : "failed";
      this.#end(reason);
    }
  }

  /**
   * A `ring` notify arrived (app pulls `call_id` out of the `notify.event`
   * payload). Subscribes to watch the call so a caller-cancel or ring-timeout
   * reaches us as an `ended` snapshot. A ring while already busy is declined.
   */
  onRing(callId: string): void {
    if (this.#view.phase === "ended") this.#view = CallManager.#idle(); // a stale ended card must not swallow a fresh ring
    if (this.#view.phase !== "idle") {
      void this.#conn.cmd({ cmd: "calls.decline", payload: { call_id: callId } }).catch(() => {});
      return;
    }
    this.#view = { ...CallManager.#idle(), phase: "ringing", isCaller: false, callId };
    this.#onChange(this.#view);
    void this.#watch(callId);
  }

  /** Accept the incoming call. The `active` snapshot then wires up media. */
  async accept(): Promise<void> {
    const callId = this.#view.callId;
    if (this.#view.phase !== "ringing" || !callId) return;
    try {
      await this.#conn.cmd({ cmd: "calls.accept", payload: { call_id: callId } });
    } catch {
      this.#end("failed");
    }
  }

  /** Decline an incoming ring. */
  decline(): void {
    const callId = this.#view.callId;
    if (this.#view.phase !== "ringing" || !callId) return;
    void this.#conn.cmd({ cmd: "calls.decline", payload: { call_id: callId } }).catch(() => {});
    this.#end("declined");
  }

  /** Hang up an outgoing/active call. Tears down locally immediately. */
  hangup(): void {
    const callId = this.#view.callId;
    if (callId && (this.#view.phase === "calling" || this.#view.phase === "active")) {
      void this.#conn.cmd({ cmd: "calls.hangup", payload: { call_id: callId } }).catch(() => {});
    }
    this.#end("ended");
  }

  /** Dismiss a terminal (`ended`) call, returning to `idle`. */
  clear(): void {
    if (this.#view.phase !== "ended") return;
    this.#view = CallManager.#idle();
    this.#onChange(this.#view);
  }

  /** Tear everything down (call `conn.close()` separately). */
  dispose(): void {
    this.#teardown();
    this.#view = CallManager.#idle();
  }

  // ── internals ───────────────────────────────────────────────────────────────

  async #watch(callId: string): Promise<void> {
    const topic = `call:${callId}`;
    this.#offCall = this.#conn.on(topic, (push) => this.#onPush(push));
    // snapshot-on-sub delivers the current `calls.state` through the handler
    await this.#conn.sub(topic).catch(() => {});
  }

  #onPush(push: Push): void {
    if (push.evt === "calls.state") this.#onState(push.payload);
    else if (push.evt === "calls.signal") this.#onSignal(push.payload);
  }

  #onState(p: {
    call_id: string;
    kind: CallKind;
    state: CallSessionState;
    participants: CallParticipant[];
    ice_servers: unknown;
  }): void {
    if (p.call_id !== this.#view.callId) return;
    this.#view.kind = p.kind;
    const peer = p.participants.find((x) => x.character_id !== this.#selfId);
    if (peer) this.#view.peer = peer.character_id;

    if (p.state === "ended") {
      this.#end(this.#endReason(peer));
      return;
    }
    if (p.state === "active") {
      this.#wasActive = true;
      if (this.#view.phase !== "active") {
        this.#view.phase = "active";
        void this.#enterCall(this.#iceServers(p.ice_servers));
      }
    }
    this.#onChange(this.#view);
  }

  #onSignal(p: { call_id: string; from: string; to: string; payload: unknown }): void {
    if (p.call_id !== this.#view.callId || p.to !== this.#selfId) return;
    const msg = p.payload as SignalMsg;
    if (this.#link) void this.#link.handleSignal(msg);
    else this.#pendingSignals.push(msg); // link not built yet — replay after #enterCall
  }

  async #enterCall(iceServers: RTCIceServer[]): Promise<void> {
    if (this.#entering || this.#link) return;
    this.#entering = true;
    try {
      const callId = this.#view.callId;
      const stream = await this.#getMedia(this.#view.kind);
      // The call can end (hangup / ring-timeout / peer decline) while getUserMedia
      // is still resolving. If it did, stop the tracks we just acquired and bail —
      // otherwise the camera/mic stay live on an orphan stream nothing ever closes,
      // and start() would fire a `calls.signal` on a dead call.
      if (this.#view.phase !== "active" || this.#view.callId !== callId) {
        for (const t of stream.getTracks()) t.stop();
        return;
      }
      this.#view.localStream = stream;
      this.#link = new PeerLink({
        pc: this.#peerFactory({ iceServers }),
        localStream: stream,
        isOfferer: this.#view.isCaller,
        send: (m) => this.#sendSignal(m),
        onRemote: (remote) => {
          this.#view.remoteStream = remote;
          this.#onChange(this.#view);
        },
        onError: () => this.hangup(),
      });
      for (const m of this.#pendingSignals) void this.#link.handleSignal(m);
      this.#pendingSignals.length = 0;
      await this.#link.start();
      this.#onChange(this.#view);
    } catch {
      // Media denied / peer-connection build failed — end the call cleanly.
      this.hangup();
    } finally {
      this.#entering = false;
    }
  }

  #sendSignal(m: SignalMsg): void {
    const callId = this.#view.callId;
    const to = this.#view.peer;
    if (!callId || !to) return;
    void this.#conn
      .cmd({ cmd: "calls.signal", payload: { call_id: callId, to, payload: m } })
      .catch(() => {}); // a dropped signal stalls setup; the connection surfaces the close
  }

  #iceServers(raw: unknown): RTCIceServer[] {
    return Array.isArray(raw) && raw.length > 0 ? (raw as RTCIceServer[]) : this.#defaultIce;
  }

  #endReason(peer: CallParticipant | undefined): CallEndReason {
    if (peer?.state === "declined") return "declined";
    if (this.#wasActive) return "ended";
    return "no_answer"; // ringing → ended without an accept (timeout or caller cancel)
  }

  #end(reason: CallEndReason): void {
    this.#teardown();
    this.#view.phase = "ended";
    this.#view.endReason = reason;
    this.#view.remoteStream = null;
    this.#onChange(this.#view);
  }

  #teardown(): void {
    this.#link?.close();
    this.#link = null;
    for (const t of this.#view.localStream?.getTracks() ?? []) t.stop();
    this.#view.localStream = null;
    this.#pendingSignals.length = 0;
    this.#entering = false;
    this.#wasActive = false;
    if (this.#view.callId) void this.#conn.unsub(`call:${this.#view.callId}`).catch(() => {});
    this.#offCall?.();
    this.#offCall = null;
  }
}

export function createCallManager(conn: OpnConnection, opts: CallManagerOptions): CallManager {
  return new CallManager(conn, opts);
}
