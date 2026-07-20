// Shared session-mint logic — the ONE place the tenant API key is used.
//
// It stands in for the FXServer in the OPN auth chain (OPN.md §3): the key
// lives here, goes only into the Authorization header to Core, and NEVER
// reaches the browser. Used by both the dev sidecar (dev-auth/server.mjs) and
// the production server (deploy/server.mjs).
//
// In a real fork, replace the mint with the operator's own auth + key custody.
// Stdlib only: global fetch (Node 20+). No dependencies.

export const CORE_URL = process.env.OPN_CORE_URL;
const API_KEY = process.env.OPN_TENANT_API_KEY;

/** Fail fast (with a readable message, not a raw stack) if misconfigured. */
export function assertEnv() {
  if (!CORE_URL) {
    console.error("FATAL: OPN_CORE_URL is required (e.g. https://opn-core.example.com)");
    process.exit(1);
  }
  try {
    const u = new URL(CORE_URL);
    if (u.protocol !== "http:" && u.protocol !== "https:") throw new Error("bad scheme");
  } catch {
    console.error(
      `FATAL: OPN_CORE_URL must be a full URL with scheme, e.g. https://opn-core.example.com (got: ${CORE_URL})`,
    );
    process.exit(1);
  }
  if (!API_KEY) {
    console.error("FATAL: OPN_TENANT_API_KEY is required");
    process.exit(1);
  }
}

export function sendJson(res, status, body) {
  const buf = Buffer.from(JSON.stringify(body));
  res.writeHead(status, { "Content-Type": "application/json", "Content-Length": buf.length });
  res.end(buf);
}

export async function readJson(req) {
  const chunks = [];
  let size = 0;
  for await (const chunk of req) {
    size += chunk.length;
    if (size > 64 * 1024) throw new Error("body too large");
    chunks.push(chunk);
  }
  return JSON.parse(Buffer.concat(chunks).toString("utf8"));
}

/**
 * Mint a Core session for `framework_ref` — the single call that uses the
 * tenant key. Returns Core's full `{ token, session_id, character, device }`.
 * Throws on failure with `.status`/`.payload` set to Core's response so callers
 * can forward the real reason. Shared by `/join` and the lobby bot (bot.mjs).
 */
export async function mintSession(framework_ref) {
  let core;
  try {
    core = await fetch(`${CORE_URL}/v1/tenants/self/sessions`, {
      method: "POST",
      headers: { Authorization: `Bearer ${API_KEY}`, "Content-Type": "application/json" },
      body: JSON.stringify({ framework_ref }),
    });
  } catch {
    const err = new Error("core unreachable");
    err.status = 502;
    err.payload = { code: "internal", msg: "core unreachable" };
    throw err;
  }
  const payload = await core.json().catch(() => null);
  if (core.status !== 200 || !payload) {
    const err = new Error(`mint failed (${core.status})`);
    err.status = core.status || 502;
    err.payload = payload ?? { code: "internal", msg: "bad core response" };
    throw err;
  }
  return payload;
}

/** `POST /join { name }` → mint a Core session, return `{ token, session_id, character }`. */
export async function handleJoin(req, res) {
  let body;
  try {
    body = await readJson(req);
  } catch {
    return sendJson(res, 400, { code: "invalid", msg: "invalid JSON body" });
  }

  const name = body?.name;
  if (typeof name !== "string" || name.length < 1 || name.length > 128) {
    return sendJson(res, 400, { code: "invalid", msg: "name must be a string of length 1..=128" });
  }

  let payload;
  try {
    payload = await mintSession(name);
  } catch (e) {
    // Forward Core's status + body faithfully so the browser sees the real reason.
    return sendJson(res, e.status ?? 502, e.payload ?? { code: "internal", msg: "mint failed" });
  }

  console.log(`mint: name=${name} character.number=${payload.character?.number}`);
  const { token, session_id, character } = payload;
  return sendJson(res, 200, { token, session_id, character });
}
