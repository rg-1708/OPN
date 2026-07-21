import "./style.css";
import { connect, type ConnectionState, type OpnConnection, type Push } from "@opn/client";
import type { MePayload } from "@opn/contracts";
import { api, type RoomSummary } from "./api.ts";
import { mountCalls, type CallController } from "./call.ts";
import { mountRoom, type RoomController } from "./room.ts";
import { escapeHtml, stateBadge } from "./ui.ts";

// App controller (roadmap W1): name form → lobby (create/join rooms) → room
// (live chat). Owns the one connection; the room view owns its ChannelStore.

const app = document.querySelector<HTMLDivElement>("#app")!;

interface Self {
  id: string;
  name: string;
  number: string | null;
}

let conn: OpnConnection | null = null;
let me: Self | null = null;
let room: RoomController | null = null;
let calls: CallController | null = null;
let currentRoomId: string | null = null;
let lastName = "";
const roomNames = new Map<string, string>(); // id → name, for notify toasts

/** WS URL from the current page — Vite/deploy proxy `/ws` to Core. */
function wsUrl(): string {
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  return `${scheme}://${location.host}/ws`;
}

/** Resolve once the connection is live; reject if it dies first. */
function waitLive(c: OpnConnection): Promise<void> {
  return new Promise((resolve, reject) => {
    if (c.state === "live") return resolve();
    const off = c.onState((s) => {
      if (s === "live") {
        off();
        resolve();
      } else if (s === "closed" || s === "taken_over") {
        off();
        reject(new Error(s === "taken_over" ? "session opened in another tab" : "connection closed"));
      }
    });
  });
}

function disposeRoom(): void {
  room?.dispose();
  room = null;
}

function teardown(): void {
  disposeRoom();
  calls?.dispose();
  calls = null;
  conn?.close();
  conn = null;
  me = null;
  currentRoomId = null;
}

// ── session ────────────────────────────────────────────────────────────────

async function doJoin(name: string): Promise<void> {
  lastName = name;
  teardown();
  const info = await api.join(name);
  const c = connect({
    url: wsUrl(),
    token: info.token,
    // Re-mint on hard auth loss by re-joining under the same name.
    remint: async () => (await api.join(name)).token,
  });
  conn = c;
  await waitLive(c);

  const payload = (await c.cmd({ cmd: "identity.me" })) as MePayload;
  me = {
    id: payload.character.id,
    name: payload.character.framework_ref,
    number: payload.character.number,
  };
  // Presence dots only mean something if this character shares presence.
  await c.cmd({ cmd: "identity.set_share_presence", payload: { on: true } }).catch(() => {});
  // 1:1 call overlay — owns one CallManager for the session; rings route here.
  calls = mountCalls(c, me.id);
  // Notify surface: chat activity (class `alert`) and incoming call rings (class
  // `ring`) both push to this topic. (A chat message never notifies an *online*
  // member — Core skips them — so alerts stay quiet between two open tabs.)
  const notifyTopic = `notify:${payload.device.id}`;
  c.on(notifyTopic, onNotify);
  await c.sub(notifyTopic).catch(() => {});

  c.onState(onGlobalState);
  showLobby();
}

function onGlobalState(s: ConnectionState): void {
  const badge = document.querySelector("#conn-state");
  if (badge) badge.innerHTML = stateBadge(s);
  if (s === "taken_over") {
    teardown();
    showForm("This session was taken over by another tab. Rejoin to continue here.");
  }
}

function onNotify(push: Push): void {
  if (push.evt !== "notify.event") return;
  // Incoming call ring (opn-core §10.4: notify class `ring` carries `call_id`).
  if (push.payload.class === "ring") {
    const r = push.payload.payload as
      | { call_id?: string; caller_name?: string; caller_number?: string; from?: string }
      | null;
    if (r?.call_id) {
      calls?.handleRing(r.call_id, r.caller_name ?? r.caller_number ?? r.from);
    }
    return;
  }
  const p = push.payload.payload as { channel_id?: string } | null;
  if (p?.channel_id && p.channel_id === currentRoomId) return; // you're looking at it
  const where = p?.channel_id ? (roomNames.get(p.channel_id) ?? "another room") : "an app";
  toast(`New activity in ${where}`);
}

// ── views ──────────────────────────────────────────────────────────────────

function showForm(errorMsg?: string): void {
  disposeRoom();
  app.innerHTML = `
    <main class="min-h-screen grid place-items-center bg-gray-50 text-gray-900">
      <form id="join-form" class="w-80 flex flex-col gap-3 rounded-xl bg-white p-6 shadow">
        <h1 class="text-lg font-semibold">Join the conference</h1>
        <input id="name" type="text" placeholder="your name" autocomplete="off"
          class="rounded-md border border-gray-300 px-3 py-2 outline-none focus:ring-2 focus:ring-blue-400" />
        <button id="join-btn" type="submit"
          class="rounded-md bg-blue-600 px-3 py-2 font-medium text-white hover:bg-blue-700 disabled:opacity-50">
          Join
        </button>
        ${errorMsg ? `<p class="text-sm text-red-600">${escapeHtml(errorMsg)}</p>` : ""}
      </form>
    </main>`;
  const form = app.querySelector<HTMLFormElement>("#join-form")!;
  const input = app.querySelector<HTMLInputElement>("#name")!;
  const btn = app.querySelector<HTMLButtonElement>("#join-btn")!;
  input.value = lastName;
  input.focus();
  form.addEventListener("submit", (e) => {
    e.preventDefault();
    const name = input.value.trim();
    if (!name) return;
    btn.disabled = true;
    input.disabled = true;
    btn.textContent = "Joining…";
    doJoin(name).catch((err: unknown) => {
      showForm(err instanceof Error ? err.message : "join failed");
    });
  });
}

function showLobby(errorMsg?: string): void {
  disposeRoom();
  currentRoomId = null;
  if (!me || !conn) return;
  app.innerHTML = `
    <main class="min-h-screen bg-gray-50 text-gray-900">
      <div class="mx-auto flex max-w-2xl flex-col gap-4 p-6">
        <header class="flex items-center justify-between gap-2">
          <div>
            <h1 class="text-lg font-semibold">Rooms</h1>
            <p class="text-sm text-gray-500">${escapeHtml(me.name)}${
              me.number ? ` · ${escapeHtml(me.number)}` : ""
            }</p>
          </div>
          <div class="flex items-center gap-2">
            <span id="conn-state">${stateBadge(conn.state)}</span>
            <button id="leave" class="rounded-md border border-gray-300 px-3 py-1.5 text-sm hover:bg-gray-100">Leave</button>
          </div>
        </header>

        <form id="create" class="flex gap-2">
          <input id="room-name" type="text" placeholder="new room name" autocomplete="off"
            class="flex-1 rounded-md border border-gray-300 px-3 py-2 outline-none focus:ring-2 focus:ring-blue-400" />
          <button type="submit" class="rounded-md bg-blue-600 px-4 py-2 font-medium text-white hover:bg-blue-700">Create</button>
        </form>
        ${errorMsg ? `<p class="text-sm text-red-600">${escapeHtml(errorMsg)}</p>` : ""}

        <section class="rounded-xl bg-white shadow">
          <div class="flex items-center justify-between border-b border-gray-100 px-4 py-2">
            <h2 class="text-sm font-semibold text-gray-600">Open rooms</h2>
            <button id="refresh" class="text-sm text-blue-600 hover:underline">Refresh</button>
          </div>
          <ul id="room-list" class="divide-y divide-gray-100">
            <li class="px-4 py-3 text-sm text-gray-400">Loading…</li>
          </ul>
        </section>
      </div>
    </main>`;

  app.querySelector<HTMLButtonElement>("#leave")!.addEventListener("click", () => {
    teardown();
    showForm();
  });
  app.querySelector<HTMLButtonElement>("#refresh")!.addEventListener("click", () => void loadRooms());

  const createForm = app.querySelector<HTMLFormElement>("#create")!;
  const roomInput = app.querySelector<HTMLInputElement>("#room-name")!;
  createForm.addEventListener("submit", (e) => {
    e.preventDefault();
    const name = roomInput.value.trim();
    if (!name || !me) return;
    api
      .createRoom(name, me.id, me.name, me.number)
      .then((r) => enterRoom(r.id, r.name))
      .catch((err: unknown) => showLobby(err instanceof Error ? err.message : "could not create room"));
  });

  // Enter a room from the list (event-delegated).
  app.querySelector<HTMLUListElement>("#room-list")!.addEventListener("click", (e) => {
    const btn = (e.target as HTMLElement).closest<HTMLButtonElement>("button[data-id]");
    if (!btn) return;
    enterRoom(btn.dataset.id!, btn.dataset.name!);
  });

  void loadRooms();
}

async function loadRooms(): Promise<void> {
  const list = app.querySelector<HTMLUListElement>("#room-list");
  if (!list) return;
  let rooms: RoomSummary[];
  try {
    rooms = await api.listRooms();
  } catch {
    list.innerHTML = `<li class="px-4 py-3 text-sm text-red-600">Could not load rooms.</li>`;
    return;
  }
  for (const r of rooms) roomNames.set(r.id, r.name);
  list.innerHTML = rooms.length
    ? rooms
        .map(
          (r) => `
        <li class="flex items-center justify-between px-4 py-3">
          <div>
            <div class="font-medium">${escapeHtml(r.name)}</div>
            <div class="text-xs text-gray-400">${r.member_count} member${r.member_count === 1 ? "" : "s"}</div>
          </div>
          <button data-id="${escapeHtml(r.id)}" data-name="${escapeHtml(r.name)}"
            class="rounded-md border border-gray-300 px-3 py-1.5 text-sm hover:bg-gray-100">Enter</button>
        </li>`,
        )
        .join("")
    : `<li class="px-4 py-6 text-center text-sm text-gray-400">No rooms yet — create one above.</li>`;
}

function enterRoom(roomId: string, name: string): void {
  if (!me || !conn) return;
  const c = conn;
  const self = me;
  roomNames.set(roomId, name);
  Promise.all([api.joinRoom(roomId, self.id, self.name, self.number), api.members(roomId)])
    .then(([, members]) => {
      disposeRoom();
      currentRoomId = roomId;
      room = mountRoom({
        el: app,
        conn: c,
        roomId,
        roomName: name,
        self: { id: self.id, name: self.name },
        members,
        onLeave: () => showLobby(),
        onCall: (number, video, label) => calls?.startCall(number, video, label),
      });
    })
    .catch((err: unknown) => showLobby(err instanceof Error ? err.message : "could not enter room"));
}

// ── toast ────────────────────────────────────────────────────────────────────

let toastHost: HTMLDivElement | null = null;
function toast(msg: string): void {
  if (!toastHost) {
    toastHost = document.createElement("div");
    toastHost.className = "fixed bottom-4 right-4 z-50 flex flex-col gap-2";
    document.body.appendChild(toastHost);
  }
  const el = document.createElement("div");
  el.className = "rounded-md bg-gray-900 px-4 py-2 text-sm text-white shadow-lg";
  el.textContent = msg;
  toastHost.appendChild(el);
  setTimeout(() => el.remove(), 4_000);
}

showForm();
