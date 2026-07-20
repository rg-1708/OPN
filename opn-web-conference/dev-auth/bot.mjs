// The lobby bot — the ONE Core WS client dev-auth holds (roadmap W1).
//
// Core gates channel membership: `channels.create`/`member_add` are WS commands,
// and `member_add` requires the ACTOR to already be a member. So an open lobby
// where strangers create and join rooms needs an authority that is a member of
// every room. That authority is this bot: a single `__lobby__` character with a
// live WS session that creates each room and adds every joiner. dev-auth's
// in-memory roster (rooms.mjs) is then the member-list source of truth, since
// the bot performs every add.
//
// In a real fork this whole mechanism is replaced by the operator's own lobby.
// Stdlib only: global WebSocket + fetch (Node 22+). No dependencies.

import { CORE_URL, mintSession } from "./join.mjs";

const BOT_REF = "__lobby__";
const AUTH_TIMEOUT_MS = 5_000;
const CMD_TIMEOUT_MS = 10_000;

let current = null; // the live bot handle, or null
let connecting = null; // in-flight connect promise (dedupes concurrent callers)

function coreWsUrl() {
  // http(s)://host → ws(s)://host, then Core's gateway path.
  return CORE_URL.replace(/^http/, "ws") + "/ws";
}

/** Open a WS, auth as the bot, and return a handle exposing `send(cmd, payload)`. */
async function makeBot() {
  const minted = await mintSession(BOT_REF); // { token, session_id, character, ... }
  const ws = new WebSocket(coreWsUrl());
  const pending = new Map();
  let nextId = 1;
  const handle = { ws };

  const failAll = (why) => {
    for (const p of pending.values()) {
      clearTimeout(p.timer);
      p.reject(new Error(why));
    }
    pending.clear();
    if (current === handle) current = null;
  };

  ws.onmessage = (ev) => {
    if (typeof ev.data !== "string") return;
    let msg;
    try {
      msg = JSON.parse(ev.data);
    } catch {
      return;
    }
    if (!msg || typeof msg !== "object" || !("reply_to" in msg)) return; // ignore pushes
    const p = pending.get(msg.reply_to);
    if (!p) return;
    pending.delete(msg.reply_to);
    clearTimeout(p.timer);
    if (msg.ok) {
      p.resolve(msg.payload);
    } else {
      const err = new Error(`core ${msg.err?.code ?? "error"}: ${msg.err?.msg ?? ""}`);
      err.code = msg.err?.code;
      p.reject(err);
    }
  };
  ws.onerror = () => {}; // a close always follows and drives recovery

  handle.send = (cmd, payload) =>
    new Promise((resolve, reject) => {
      if (ws.readyState !== WebSocket.OPEN) return reject(new Error("bot ws not open"));
      const id = nextId++;
      const timer = setTimeout(() => {
        pending.delete(id);
        reject(new Error(`bot cmd "${cmd}" timed out`));
      }, CMD_TIMEOUT_MS);
      pending.set(id, { resolve, reject, timer });
      ws.send(JSON.stringify(payload === undefined ? { id, cmd } : { id, cmd, payload }));
    });

  // Open, then send `auth` as the first frame within Core's window (§4.1).
  await new Promise((resolve, reject) => {
    const to = setTimeout(() => {
      failAll("bot auth timed out");
      reject(new Error("bot auth timed out"));
    }, AUTH_TIMEOUT_MS);
    ws.onopen = () => {
      handle.send("auth", { token: minted.token }).then(
        () => {
          clearTimeout(to);
          resolve();
        },
        (e) => {
          clearTimeout(to);
          reject(e);
        },
      );
    };
    ws.onclose = () => {
      clearTimeout(to);
      failAll("bot ws closed before auth");
      reject(new Error("bot ws closed before auth"));
    };
  });

  // Steady state: a later close just drops the handle so the next call reconnects.
  ws.onclose = () => failAll("bot ws closed");
  return handle;
}

function ensure() {
  if (current && current.ws.readyState === WebSocket.OPEN) return Promise.resolve(current);
  if (!connecting) {
    connecting = makeBot()
      .then((h) => {
        current = h;
        connecting = null;
        return h;
      })
      .catch((e) => {
        connecting = null;
        throw e;
      });
  }
  return connecting;
}

/**
 * Run a Core command as the lobby bot, awaiting its ack. Reconnects lazily if
 * the socket is down. Rejects with an `Error` carrying `.code` (Core's ErrCode)
 * on an `ok:false` ack.
 */
export async function botCmd(cmd, payload) {
  const h = await ensure();
  try {
    return await h.send(cmd, payload);
  } catch (e) {
    if (current === h && h.ws.readyState !== WebSocket.OPEN) current = null;
    throw e;
  }
}
