// Room roster + lobby HTTP (roadmap W1). Rooms are Core group channels; this
// module keeps only an in-memory index of them (id → name → members) so the
// browser has something to discover and join. It is a TEST RIG, not a product:
// a dev-auth restart loses the lobby list — the channels themselves survive in
// Core. In a real fork this is replaced by the operator's own lobby.
//
// The actual channel create/member_add run over the lobby bot (bot.mjs), the
// only member-authority the open-join flow can use. Stdlib only.

import { botCmd } from "./bot.mjs";
import { readJson, sendJson } from "./join.mjs";

/** roomId → { name, members: Map<characterId, { name, number }> }. Bot is never a member here. */
const rooms = new Map();

function isUuid(s) {
  return typeof s === "string" && /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(s);
}

function isName(s) {
  return typeof s === "string" && s.length >= 1 && s.length <= 128;
}

/** Phone number is optional (a character may have none) — accept a short string or null. */
function isNumberOrNull(s) {
  return s === null || s === undefined || (typeof s === "string" && s.length <= 64);
}

/** Map a bot/Core error to an HTTP status + body. */
function coreError(res, e) {
  const status =
    { forbidden: 403, not_found: 404, invalid: 400, conflict: 409 }[e?.code] ?? 502;
  return sendJson(res, status, { code: e?.code ?? "internal", msg: e?.message ?? "lobby error" });
}

function summary(id, room) {
  return { id, name: room.name, member_count: room.members.size };
}

/** `GET /rooms` → the lobby list. `POST /rooms { name, character_id, character_name }` → create. */
export async function handleRooms(req, res) {
  if (req.method === "GET") {
    const list = [...rooms.entries()].map(([id, room]) => summary(id, room));
    return sendJson(res, 200, { rooms: list });
  }
  if (req.method !== "POST") {
    return sendJson(res, 405, { code: "invalid", msg: "GET or POST /rooms" });
  }

  let body;
  try {
    body = await readJson(req);
  } catch {
    return sendJson(res, 400, { code: "invalid", msg: "invalid JSON body" });
  }
  const { name, character_id, character_name, character_number = null } = body ?? {};
  if (!isName(name)) return sendJson(res, 400, { code: "invalid", msg: "name 1..=128 chars" });
  if (!isUuid(character_id)) return sendJson(res, 400, { code: "invalid", msg: "character_id must be a UUID" });
  if (!isName(character_name)) return sendJson(res, 400, { code: "invalid", msg: "character_name 1..=128 chars" });
  if (!isNumberOrNull(character_number)) return sendJson(res, 400, { code: "invalid", msg: "character_number must be a string or null" });

  try {
    // Bot creates the group (bot = creator + first member), then adds the
    // requesting character so they're a member of their own room.
    const created = await botCmd("channels.create", { name, members: [] });
    const id = created?.channel_id;
    if (!isUuid(id)) throw new Error("core returned no channel_id");
    await botCmd("channels.member_add", { channel_id: id, character_id });
    const room = { name, members: new Map([[character_id, { name: character_name, number: character_number }]]) };
    rooms.set(id, room);
    console.log(`room created: ${name} (${id}) by ${character_name}`);
    return sendJson(res, 200, summary(id, room));
  } catch (e) {
    return coreError(res, e);
  }
}

/** `POST /rooms/:id/join { character_id, character_name }` → bot adds the joiner. */
export async function handleRoomJoin(req, res, roomId) {
  if (req.method !== "POST") {
    return sendJson(res, 405, { code: "invalid", msg: "POST /rooms/:id/join" });
  }
  const room = rooms.get(roomId);
  if (!room) return sendJson(res, 404, { code: "not_found", msg: "unknown room" });

  let body;
  try {
    body = await readJson(req);
  } catch {
    return sendJson(res, 400, { code: "invalid", msg: "invalid JSON body" });
  }
  const { character_id, character_name, character_number = null } = body ?? {};
  if (!isUuid(character_id)) return sendJson(res, 400, { code: "invalid", msg: "character_id must be a UUID" });
  if (!isName(character_name)) return sendJson(res, 400, { code: "invalid", msg: "character_name 1..=128 chars" });
  if (!isNumberOrNull(character_number)) return sendJson(res, 400, { code: "invalid", msg: "character_number must be a string or null" });

  try {
    await botCmd("channels.member_add", { channel_id: roomId, character_id });
    room.members.set(character_id, { name: character_name, number: character_number });
    console.log(`room join: ${character_name} → ${room.name} (${roomId})`);
    return sendJson(res, 200, { ok: true, room: summary(roomId, room) });
  } catch (e) {
    return coreError(res, e);
  }
}

/** `GET /rooms/:id/members` → `[{ character_id, name, number }]` (the bot is excluded). */
export async function handleRoomMembers(req, res, roomId) {
  if (req.method !== "GET") {
    return sendJson(res, 405, { code: "invalid", msg: "GET /rooms/:id/members" });
  }
  const room = rooms.get(roomId);
  if (!room) return sendJson(res, 404, { code: "not_found", msg: "unknown room" });
  const members = [...room.members.entries()].map(([character_id, m]) => ({
    character_id,
    name: m.name,
    number: m.number ?? null,
  }));
  return sendJson(res, 200, { members });
}

/**
 * Route `/rooms...` requests. Returns `true` if it handled the request.
 * Kept as one matcher so both dev-auth and the prod server wire it identically.
 */
export function routeRooms(req, res, pathname) {
  if (pathname === "/rooms") {
    void handleRooms(req, res);
    return true;
  }
  const join = pathname.match(/^\/rooms\/([^/]+)\/join$/);
  if (join) {
    void handleRoomJoin(req, res, decodeURIComponent(join[1]));
    return true;
  }
  const members = pathname.match(/^\/rooms\/([^/]+)\/members$/);
  if (members) {
    void handleRoomMembers(req, res, decodeURIComponent(members[1]));
    return true;
  }
  return false;
}
