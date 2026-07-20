#!/usr/bin/env node
// W1 smoke (roadmap W1 test plan): against a real dockerized Core, mint two
// sessions, create a group channel that already contains both, subscribe each
// side with a ChannelStore, and prove live chat round-trips — optimistic send
// reconciles (no duplicate) and both sides converge on the same seq-ordered log.
// Zero dependencies — Node 26's global fetch + WebSocket, and the client TS
// source loaded via type-stripping.
//
//   OPN_CORE_URL=http://localhost:8080 OPN_TENANT_API_KEY=opn_... node scripts/smoke-w1.mjs
import { connect, createChannelStore } from "../packages/client/src/index.ts";

const CORE = process.env.OPN_CORE_URL ?? "http://localhost:8080";
const KEY = process.env.OPN_TENANT_API_KEY;
if (!KEY) {
  console.error("OPN_TENANT_API_KEY is required");
  process.exit(2);
}
const wsUrl = CORE.replace(/^http/, "ws") + "/ws";

/** Mint a session the way the dev-auth sidecar does (this stands in for /join). */
async function mint(name) {
  const res = await fetch(`${CORE}/v1/tenants/self/sessions`, {
    method: "POST",
    headers: { authorization: `Bearer ${KEY}`, "content-type": "application/json" },
    body: JSON.stringify({ framework_ref: name }),
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

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Poll `check` every 50ms up to `ms`; returns true if it ever passed. */
async function poll(check, ms) {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    if (check()) return true;
    await sleep(50);
  }
  return check();
}

async function main() {
  const tag = Date.now().toString(36);

  // 1. Mint two sessions with distinct refs so reruns don't collide.
  const A = await mint(`smoke-a-${tag}`);
  const B = await mint(`smoke-b-${tag}`);
  const aId = A.character.id;
  const bId = B.character.id;
  console.log(`✔ minted A=${A.session_id} (${aId}) and B=${B.session_id} (${bId})`);

  // 2. Connect both and wait for `live`.
  const connA = connect({ url: wsUrl, token: A.token, remint: async () => (await mint(`smoke-a-${tag}`)).token });
  const connB = connect({ url: wsUrl, token: B.token, remint: async () => (await mint(`smoke-b-${tag}`)).token });
  await waitFor(connA, "live", 5_000);
  await waitFor(connB, "live", 5_000);
  console.log("✔ both connections live");

  // 3. A creates a group channel that already contains B — both are members
  //    from the start, so no join-permission dance.
  const ack = await connA.cmd({ cmd: "channels.create", payload: { name: `smoke-${tag}`, members: [bId] } });
  const channelId = ack.channel_id;
  if (typeof channelId !== "string") {
    throw new Error(`channels.create returned no channel_id: ${JSON.stringify(ack)}`);
  }
  console.log(`✔ created channel ${channelId}`);

  // 4. A store per side, both subscribed.
  const storeA = createChannelStore(connA, channelId, { selfId: aId, onChange: () => {} });
  const storeB = createChannelStore(connB, channelId, { selfId: bId, onChange: () => {} });
  await storeA.subscribe();
  await storeB.subscribe();
  console.log("✔ both sides subscribed");

  // 5. A sends the first message.
  storeA.send({ text: "hello from A", media_ids: null, gif_url: null, meta: null });

  // 6. Wait until both sides show exactly the one reconciled message.
  const firstOk = () => {
    const a = storeA.messages();
    const b = storeB.messages();
    return (
      a.length === 1 &&
      a[0].status === "sent" &&
      typeof a[0].seq === "number" &&
      a[0].body.text === "hello from A" &&
      b.length === 1 &&
      b[0].body.text === "hello from A" &&
      b[0].mine === false &&
      typeof b[0].seq === "number"
    );
  };
  if (!(await poll(firstOk, 3_000))) {
    throw new Error(
      `first message did not converge:\n  A=${JSON.stringify(storeA.messages())}\n  B=${JSON.stringify(storeB.messages())}`,
    );
  }
  console.log("✔ A's message reconciled once on A and delivered once to B");

  // 7. B replies; A must receive it and ordering by seq must hold.
  storeB.send({ text: "reply from B", media_ids: null, gif_url: null, meta: null });
  if (!(await poll(() => storeA.messages().length === 2, 3_000))) {
    throw new Error(`A did not receive B's reply: A=${JSON.stringify(storeA.messages())}`);
  }
  const [m0, m1] = storeA.messages();
  if (!(m0.seq < m1.seq)) {
    throw new Error(`seqs not ascending on A: ${JSON.stringify(storeA.messages())}`);
  }
  if (m1.body.text !== "reply from B") {
    throw new Error(`A's second message is not B's reply: ${JSON.stringify(storeA.messages())}`);
  }
  console.log("✔ B's reply reached A, ordering by seq preserved");

  // 8. Clean up.
  storeA.dispose();
  storeB.dispose();
  connA.close();
  connB.close();
  console.log("✔ W1 smoke passed");
  process.exit(0);
}

main().catch((err) => {
  console.error(err.message);
  process.exit(1);
});
