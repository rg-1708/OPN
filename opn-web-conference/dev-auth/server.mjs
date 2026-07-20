// dev-auth (development sidecar) — stands in for the FXServer in the OPN auth
// chain (OPN.md §3). It is the ONLY component that holds the tenant API key and
// mints sessions so the key NEVER reaches the browser. The mint itself lives in
// join.mjs (shared with the production server). In dev, Vite proxies /join here.
//
// In a real fork, replace this + join.mjs with the operator's own auth. Rooms/
// lobby logic will be ADDED here in a later sprint (W1).
//
// Stdlib only: node:http + global fetch. No dependencies.

import { createServer } from "node:http";
import { assertEnv, CORE_URL, handleJoin, sendJson } from "./join.mjs";

assertEnv();
const PORT = Number(process.env.DEV_AUTH_PORT) || 8787;

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
  console.log(`dev-auth listening on :${PORT} — OPN_CORE_URL=${CORE_URL}`);
});
