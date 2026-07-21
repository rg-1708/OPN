import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { CLOSE, OpnSocket } from "./socket";
import { OpnError } from "./types";

class FakeWS {
  static instances: FakeWS[] = [];
  sent: any[] = [];
  onopen: (() => void) | null = null;
  onmessage: ((ev: { data: string }) => void) | null = null;
  onclose: ((ev: { code: number; reason: string }) => void) | null = null;
  constructor(public url: string) {
    FakeWS.instances.push(this);
  }
  send(s: string) {
    this.sent.push(JSON.parse(s));
  }
  close(code = 1000) {
    this.onclose?.({ code, reason: "" });
  }
  // test helpers
  open() {
    this.onopen?.();
  }
  receive(obj: unknown) {
    this.onmessage?.({ data: JSON.stringify(obj) });
  }
  serverClose(code: number, reason = "") {
    this.onclose?.({ code, reason });
  }
}

function makeSocket(token = "h.e.d") {
  return new OpnSocket({
    url: "wss://core.test/ws",
    token,
    webSocket: FakeWS as unknown as new (url: string) => WebSocket,
    minBackoffMs: 100,
  });
}

function lastWs(): FakeWS {
  return FakeWS.instances[FakeWS.instances.length - 1]!;
}

async function openAndAuth(s: OpnSocket): Promise<FakeWS> {
  void s.connect();
  await Promise.resolve();
  const ws = lastWs();
  ws.open();
  const auth = ws.sent[0];
  expect(auth.cmd).toBe("auth");
  ws.receive({ reply_to: auth.id, ok: true });
  await Promise.resolve();
  return ws;
}

beforeEach(() => {
  FakeWS.instances = [];
  vi.useFakeTimers();
});
afterEach(() => {
  vi.useRealTimers();
});

describe("OpnSocket", () => {
  it("sends auth as first frame and flushes queued cmds after auth", async () => {
    const s = makeSocket();
    void s.connect();
    const p = s.cmd("channels.list"); // queued: not authed yet
    await Promise.resolve();
    const ws = lastWs();
    ws.open();
    expect(ws.sent.length).toBe(1); // only auth so far
    ws.receive({ reply_to: ws.sent[0].id, ok: true });
    await Promise.resolve();
    expect(s.state).toBe("open");
    const listFrame = ws.sent.find((f) => f.cmd === "channels.list");
    expect(listFrame).toBeTruthy();
    ws.receive({ reply_to: listFrame.id, ok: true, payload: [] });
    await expect(p).resolves.toEqual([]);
  });

  it("rejects a nacked cmd with the wire error", async () => {
    const s = makeSocket();
    const ws = await openAndAuth(s);
    const p = s.cmd("channels.open_direct", { number: "555" });
    const frame = ws.sent.find((f) => f.cmd === "channels.open_direct");
    ws.receive({
      reply_to: frame.id,
      ok: false,
      err: { code: "not_found", msg: "no such number" },
    });
    await expect(p).rejects.toMatchObject({ code: "not_found" });
    await expect(p).rejects.toBeInstanceOf(OpnError);
  });

  it("dispatches pushes, tracks seq, and resumes subs on reconnect", async () => {
    const s = makeSocket();
    const ws = await openAndAuth(s);
    const got: unknown[] = [];
    s.on("channels.message", (payload) => got.push(payload));
    s.sub("ch:abc");
    const subFrame = ws.sent.find((f) => f.cmd === "sub");
    expect(subFrame.payload).toEqual({ topic: "ch:abc", last_seq: null });
    ws.receive({ reply_to: subFrame.id, ok: true });
    ws.receive({
      topic: "ch:abc",
      evt: "channels.message",
      payload: { channel_id: "abc", seq: 5, body: {} },
    });
    expect(got.length).toBe(1);

    ws.serverClose(1006); // network drop
    expect(s.state).toBe("closed");
    await vi.advanceTimersByTimeAsync(1000);
    const ws2 = lastWs();
    expect(ws2).not.toBe(ws);
    ws2.open();
    ws2.receive({ reply_to: ws2.sent[0].id, ok: true });
    await Promise.resolve();
    const resub = ws2.sent.find((f) => f.cmd === "sub");
    expect(resub.payload).toEqual({ topic: "ch:abc", last_seq: 5 });
  });

  it("does not reconnect after TAKEN_OVER or manual close", async () => {
    const s = makeSocket();
    const ws = await openAndAuth(s);
    const count = FakeWS.instances.length;
    ws.serverClose(CLOSE.TAKEN_OVER);
    await vi.advanceTimersByTimeAsync(60_000);
    expect(FakeWS.instances.length).toBe(count);

    const s2 = makeSocket();
    await openAndAuth(s2);
    const count2 = FakeWS.instances.length;
    s2.close();
    await vi.advanceTimersByTimeAsync(60_000);
    expect(FakeWS.instances.length).toBe(count2);
    await expect(s2.cmd("channels.list")).rejects.toMatchObject({ code: "closed" });
  });

  it("unsub is refcounted", async () => {
    const s = makeSocket();
    const ws = await openAndAuth(s);
    const un1 = s.sub("ch:x");
    const un2 = s.sub("ch:x");
    expect(ws.sent.filter((f) => f.cmd === "sub").length).toBe(1);
    un1();
    expect(ws.sent.filter((f) => f.cmd === "unsub").length).toBe(0);
    un2();
    expect(ws.sent.filter((f) => f.cmd === "unsub").length).toBe(1);
  });
});
