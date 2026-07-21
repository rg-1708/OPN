#!/usr/bin/env node
// W0 smoke (roadmap W0 test plan): against a real dockerized Core, mint a
// session, open a WS with the real @opn/client, and prove it reaches `live`.
// Zero dependencies — Node 26's global fetch + WebSocket, and the client TS
// source loaded via type-stripping.
//
//   OPN_CORE_URL=http://localhost:8080 OPN_TENANT_API_KEY=opn_... node scripts/smoke.mjs
//
// Optional: SMOKE_RESTART=1 pauses after `live` so you can `docker restart` Core
// in another terminal and watch the client go reconnecting → live, then Ctrl-C.
import { connect } from "../packages/client/src/index.ts";

const CORE = process.env.OPN_CORE_URL ?? "http://localhost:8080";
const KEY = process.env.OPN_TENANT_API_KEY;
if (!KEY) {
  console.error("OPN_TENANT_API_KEY is required");
  process.exit(2);
}
const wsUrl = CORE.replace(/^http/, "ws") + "/ws";

/** Mint a session the way the dev-auth sidecar does (this stands in for /join). */
async function mint() {
  const res = await fetch(`${CORE}/v1/tenants/self/sessions`, {
    method: "POST",
    headers: { authorization: `Bearer ${KEY}`, "content-type": "application/json" },
    body: JSON.stringify({ framework_ref: "smoke" }),
  });
  if (!res.ok) {
    throw new Error(`mint failed: ${res.status} ${await res.text()}`);
  }
  return res.json();
}

function waitFor(conn, target, ms) {
  return new Promise((resolve, reject) => {
    if (conn.state === target) return resolve();
    const timer = setTimeout(() => reject(new Error(`timed out waiting for "${target}" (last: ${conn.state})`)), ms);
    const off = conn.onState((s) => {
      if (s === target) {
        clearTimeout(timer);
        off();
        resolve();
      }
    });
  });
}

const minted = await mint();
console.log(`minted session ${minted.session_id} for ${minted.character.framework_ref} (number ${minted.character.number})`);

const conn = connect({ url: wsUrl, token: minted.token, remint: async () => (await mint()).token });
await waitFor(conn, "live", 5_000);
console.log("✔ connected and live");

if (process.env.SMOKE_RESTART) {
  console.log("SMOKE_RESTART set — restart Core now; watching for reconnect. Ctrl-C to stop.");
  conn.onState((s) => console.log(`  state → ${s}`));
} else {
  conn.close();
  console.log("✔ smoke passed");
  process.exit(0);
}
