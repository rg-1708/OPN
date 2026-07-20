// dev-auth — stands in for the FXServer in the OPN auth chain (OPN.md §3).
//
// This is the ONLY component that holds the tenant API key. It mints a session
// on behalf of a browser client so the key NEVER reaches the browser: the key
// lives here, in the Authorization header to Core, and nowhere else.
//
// In a real fork, replace this file with the operator's own auth (their own
// login, their own key custody). Rooms/lobby logic will be ADDED here in a
// later sprint (W1) — kept intentionally small and clean for that.
//
// Stdlib only: node:http + global fetch (Node 26). No dependencies.

import { createServer } from "node:http";

const { OPN_CORE_URL, OPN_TENANT_API_KEY } = process.env;
const PORT = Number(process.env.DEV_AUTH_PORT) || 8787;

if (!OPN_CORE_URL) {
  console.error("FATAL: OPN_CORE_URL is required (e.g. http://localhost:8080)");
  process.exit(1);
}
if (!OPN_TENANT_API_KEY) {
  console.error("FATAL: OPN_TENANT_API_KEY is required");
  process.exit(1);
}

function sendJson(res, status, body) {
  const buf = Buffer.from(JSON.stringify(body));
  res.writeHead(status, {
    "Content-Type": "application/json",
    "Content-Length": buf.length,
  });
  res.end(buf);
}

async function readJson(req) {
  const chunks = [];
  let size = 0;
  for await (const chunk of req) {
    size += chunk.length;
    if (size > 64 * 1024) throw new Error("body too large");
    chunks.push(chunk);
  }
  return JSON.parse(Buffer.concat(chunks).toString("utf8"));
}

async function handleJoin(req, res) {
  let body;
  try {
    body = await readJson(req);
  } catch {
    return sendJson(res, 400, { code: "invalid", msg: "invalid JSON body" });
  }

  const name = body?.name;
  if (typeof name !== "string" || name.length < 1 || name.length > 128) {
    return sendJson(res, 400, {
      code: "invalid",
      msg: "name must be a string of length 1..=128",
    });
  }

  let core;
  try {
    core = await fetch(`${OPN_CORE_URL}/v1/tenants/self/sessions`, {
      method: "POST",
      headers: {
        Authorization: `Bearer ${OPN_TENANT_API_KEY}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ framework_ref: name }),
    });
  } catch {
    return sendJson(res, 502, { code: "internal", msg: "core unreachable" });
  }

  const payload = await core.json().catch(() => null);

  // Forward Core errors faithfully so the browser sees the real reason.
  if (core.status !== 200 || !payload) {
    return sendJson(res, core.status || 502, payload ?? {
      code: "internal",
      msg: "bad core response",
    });
  }

  console.log(`mint: name=${name} character.number=${payload.character?.number}`);
  const { token, session_id, character } = payload;
  return sendJson(res, 200, { token, session_id, character });
}

const server = createServer((req, res) => {
  if (req.method === "GET" && req.url === "/healthz") {
    return sendJson(res, 200, { ok: true });
  }
  if (req.url === "/join") {
    if (req.method !== "POST") {
      return sendJson(res, 405, { code: "invalid", msg: "use POST /join" });
    }
    return handleJoin(req, res);
  }
  return sendJson(res, 404, { code: "invalid", msg: "not found" });
});

server.listen(PORT, () => {
  console.log(`dev-auth listening on :${PORT} — OPN_CORE_URL=${OPN_CORE_URL}`);
});
