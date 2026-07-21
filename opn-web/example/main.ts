// Minimal vanilla-DOM consumer of @opn/client. This is a smoke tool, not a UI
// to copy — see GUIDE.md for how to build a real frontend on the client.
//
// Usage: get a session JWT (minted by your game server via
// POST /v1/tenants/self/sessions) and open:
//   http://localhost:5173/?token=<jwt>&core=wss://localhost:8080/ws
import { createOpnClient, topics, type EvtName } from "../src/index";

const params = new URLSearchParams(location.search);
const token = params.get("token") ?? prompt("session JWT") ?? "";
const url = params.get("core") ?? "ws://127.0.0.1:8080/ws";

const $ = (id: string) => document.getElementById(id)!;
const log = (line: string) => {
  $("log").textContent += line + "\n";
};

const { socket, http } = createOpnClient({ url, token });

socket.onState((s) => ($("status").textContent = s));
socket.onError((e) => log(`! ${e.message}`));

// Log every event on the channel we're viewing.
let unsubTopic: (() => void) | null = null;
let activeChannel: string | null = null;

async function openChannel(id: string) {
  unsubTopic?.();
  activeChannel = id;
  $("send").hidden = false;
  document.querySelectorAll("li").forEach((li) =>
    li.classList.toggle("active", li.dataset.id === id),
  );
  const sub = socket.sub(topics.channel(id));
  const onEvt = socket.onTopic(topics.channel(id), (evt: EvtName, payload) =>
    log(`${evt} ${JSON.stringify(payload)}`),
  );
  unsubTopic = () => {
    sub();
    onEvt();
  };
  const history = await http.channelMessages(id, { limit: 20 });
  for (const m of [...history].reverse()) {
    log(`  [${m.seq}] ${JSON.stringify(m.body)}`);
  }
}

$("send").addEventListener("submit", async (e) => {
  e.preventDefault();
  const input = $("text") as HTMLInputElement;
  if (!activeChannel || !input.value) return;
  await socket.cmd("channels.send", {
    channel_id: activeChannel,
    client_uuid: crypto.randomUUID(),
    body: { text: input.value, media_ids: null, gif_url: null, meta: null },
  });
  input.value = "";
});

await socket.connect();
const me = await socket.cmd("identity.me");
log(`me: ${me.character.number ?? me.character.id}`);

for (const ch of await socket.cmd("channels.list")) {
  const li = document.createElement("li");
  li.dataset.id = ch.channel_id;
  li.textContent = `${ch.name ?? ch.kind} (last_seq ${ch.last_seq})`;
  li.onclick = () => void openChannel(ch.channel_id);
  $("channels").append(li);
}
