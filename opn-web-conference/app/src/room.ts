import {
  createChannelStore,
  type ChannelStore,
  type ChatMessage,
  type ConnectionState,
  type OpnConnection,
} from "@opn/client";
import type { MessageBody } from "@opn/contracts";
import { api, type RoomMember } from "./api.ts";
import { escapeHtml, stateBadge } from "./ui.ts";

export interface RoomOptions {
  el: HTMLElement;
  conn: OpnConnection;
  roomId: string;
  roomName: string;
  self: { id: string; name: string };
  members: RoomMember[];
  onLeave: () => void;
}

export interface RoomController {
  dispose: () => void;
}

/** RFC 3339 → HH:MM, or "…" while a message is still optimistic. */
function clock(at: string | null): string {
  if (!at) return "…";
  const d = new Date(at);
  return Number.isNaN(d.getTime())
    ? "…"
    : d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

function bodyText(body: unknown): string {
  const t = (body as MessageBody | null)?.text;
  return typeof t === "string" ? t : "";
}

/**
 * Mount the room chat view: a live message list off a `ChannelStore`, a member
 * sidebar with presence dots, typing indicator, and watermark receipts. Renders
 * the shell once, then patches the message/member/typing regions on change so
 * the composer never loses focus. Returns a controller whose `dispose()` tears
 * down the store, presence subscriptions, and handlers.
 */
export function mountRoom(opts: RoomOptions): RoomController {
  const { el, conn, roomId, roomName, self, onLeave } = opts;

  // characterId → display name (seeded from dev-auth, refreshed on membership change).
  const names = new Map<string, string>(opts.members.map((m) => [m.character_id, m.name]));
  names.set(self.id, self.name);
  // characterId → online (true/false, or null = doesn't share presence / unknown).
  const online = new Map<string, boolean | null>();
  // characterId → cleanup for its presence watch.
  const presenceWatch = new Map<string, () => void>();

  el.innerHTML = `
    <main class="flex h-screen flex-col bg-gray-50 text-gray-900">
      <header class="flex items-center gap-3 border-b border-gray-200 bg-white px-4 py-3">
        <button id="back" class="rounded-md border border-gray-300 px-2 py-1 text-sm hover:bg-gray-100">← Lobby</button>
        <h1 class="flex-1 truncate text-lg font-semibold">${escapeHtml(roomName)}</h1>
        <span id="state">${stateBadge(conn.state)}</span>
      </header>
      <div class="flex min-h-0 flex-1">
        <section class="flex min-h-0 flex-1 flex-col">
          <div id="messages" class="flex flex-1 flex-col gap-2 overflow-y-auto p-4"></div>
          <div id="typing" class="h-5 px-4 text-xs italic text-gray-500"></div>
          <form id="composer" class="flex gap-2 border-t border-gray-200 bg-white p-3">
            <input id="msg" type="text" autocomplete="off" placeholder="Message…"
              class="flex-1 rounded-md border border-gray-300 px-3 py-2 outline-none focus:ring-2 focus:ring-blue-400" />
            <button type="submit" class="rounded-md bg-blue-600 px-4 py-2 font-medium text-white hover:bg-blue-700">Send</button>
          </form>
        </section>
        <aside id="members" class="w-56 shrink-0 overflow-y-auto border-l border-gray-200 bg-white p-3"></aside>
      </div>
    </main>`;

  const messagesEl = el.querySelector<HTMLDivElement>("#messages")!;
  const membersEl = el.querySelector<HTMLDivElement>("#members")!;
  const typingEl = el.querySelector<HTMLDivElement>("#typing")!;
  const stateEl = el.querySelector<HTMLSpanElement>("#state")!;
  const form = el.querySelector<HTMLFormElement>("#composer")!;
  const input = el.querySelector<HTMLInputElement>("#msg")!;

  const store: ChannelStore = createChannelStore(conn, roomId, {
    selfId: self.id,
    onChange: render,
  });

  function nameOf(id: string): string {
    return names.get(id) ?? `${id.slice(0, 8)}…`;
  }

  function renderMessages(): void {
    // Highest read watermark across peers → a "read" tick on our own messages.
    let peerRead = 0;
    for (const r of store.receipts().values()) peerRead = Math.max(peerRead, r.read);

    const nearBottom =
      messagesEl.scrollHeight - messagesEl.scrollTop - messagesEl.clientHeight < 40;

    messagesEl.innerHTML = store
      .messages()
      .map((m) => renderMessage(m, peerRead))
      .join("");

    if (nearBottom) messagesEl.scrollTop = messagesEl.scrollHeight;
  }

  function renderMessage(m: ChatMessage, peerRead: number): string {
    const text = bodyText(m.body);
    const bubble = m.mine
      ? "bg-blue-600 text-white"
      : "bg-white text-gray-900 ring-1 ring-gray-200";
    const senderLine = m.mine
      ? ""
      : `<div class="mb-0.5 text-xs font-semibold text-gray-500">${escapeHtml(nameOf(m.sender))}</div>`;
    let meta = clock(m.at);
    if (m.mine) {
      if (m.status === "sending") meta = "sending…";
      else if (m.status === "failed") meta = "failed — will retry on reconnect";
      else meta = `${clock(m.at)} · ${m.seq !== null && peerRead >= m.seq ? "read ✓✓" : "sent ✓"}`;
    }
    const failed = m.status === "failed" ? " opacity-60" : "";
    return `
      <div class="flex flex-col ${m.mine ? "items-end" : "items-start"}">
        <div class="max-w-[75%] rounded-2xl px-3 py-2 text-sm${failed} ${bubble}">
          ${senderLine}<div class="whitespace-pre-wrap break-words">${escapeHtml(text)}</div>
        </div>
        <div class="mt-0.5 text-[10px] text-gray-400">${escapeHtml(meta)}</div>
      </div>`;
  }

  function renderMembers(): void {
    const rows = [...names.entries()]
      .sort((a, b) => a[1].localeCompare(b[1]))
      .map(([id, name]) => {
        const isSelf = id === self.id;
        const on = isSelf ? true : online.get(id);
        const dot =
          on === true ? "bg-green-500" : on === false ? "bg-gray-300" : "bg-gray-200 ring-1 ring-gray-300";
        return `
          <li class="flex items-center gap-2 py-1 text-sm">
            <span class="h-2.5 w-2.5 rounded-full ${dot}"></span>
            <span class="truncate">${escapeHtml(name)}${isSelf ? ` <span class="text-gray-400">(you)</span>` : ""}</span>
          </li>`;
      })
      .join("");
    membersEl.innerHTML = `
      <h2 class="mb-2 text-xs font-semibold uppercase tracking-wide text-gray-500">Members (${names.size})</h2>
      <ul>${rows}</ul>`;
  }

  function renderTyping(): void {
    const who = store.typingUsers().map(nameOf);
    typingEl.textContent =
      who.length === 0
        ? ""
        : who.length === 1
          ? `${who[0]} is typing…`
          : `${who.join(", ")} are typing…`;
  }

  function render(): void {
    renderMessages();
    renderMembers();
    renderTyping();
    if (document.hasFocus()) store.markRead();
  }

  // ── presence ────────────────────────────────────────────────────────────────
  function watchPresence(id: string): void {
    if (id === self.id || presenceWatch.has(id)) return;
    const topic = `presence:${id}`;
    const off = conn.on(topic, (push) => {
      if (push.evt !== "presence.state") return;
      online.set(id, push.payload.online);
      renderMembers();
    });
    void conn.sub(topic).catch(() => {}); // snapshot arrives via the handler
    presenceWatch.set(id, () => {
      off();
      void conn.unsub(topic).catch(() => {});
    });
  }

  function unwatchPresence(id: string): void {
    presenceWatch.get(id)?.();
    presenceWatch.delete(id);
    online.delete(id);
  }

  async function refreshMembers(): Promise<void> {
    try {
      const list = await api.members(roomId);
      const next = new Set(list.map((m) => m.character_id));
      names.clear();
      names.set(self.id, self.name);
      for (const m of list) names.set(m.character_id, m.name);
      for (const id of next) watchPresence(id);
      for (const id of [...presenceWatch.keys()]) if (!next.has(id)) unwatchPresence(id);
      renderMembers();
    } catch {
      /* transient; the next membership event retries */
    }
  }

  // ── live wiring ──────────────────────────────────────────────────────────────
  // Membership changes come on the channel topic (the store ignores them).
  const offMember = conn.on(`ch:${roomId}`, (push) => {
    if (push.evt === "channels.member") void refreshMembers();
  });
  const offState = conn.onState((s: ConnectionState) => {
    stateEl.innerHTML = stateBadge(s);
  });

  form.addEventListener("submit", (e) => {
    e.preventDefault();
    const text = input.value.trim();
    if (!text) return;
    store.send({ text, media_ids: null, gif_url: null, meta: null });
    input.value = "";
    input.focus();
  });
  input.addEventListener("input", () => store.typing());
  const onWindowFocus = (): void => store.markRead();
  window.addEventListener("focus", onWindowFocus);

  el.querySelector<HTMLButtonElement>("#back")!.addEventListener("click", () => onLeave());

  // ── boot ─────────────────────────────────────────────────────────────────────
  for (const m of opts.members) watchPresence(m.character_id);
  renderMembers();
  input.focus();

  void store
    .subscribe()
    .then(() => api.history(roomId, conn.token))
    .then((page) => {
      store.ingestHistory(page); // newest-first page; the store sorts by seq
      render();
    })
    .catch(() => render()); // a forbidden sub or empty history still shows the shell

  return {
    dispose(): void {
      store.dispose();
      offMember();
      offState();
      window.removeEventListener("focus", onWindowFocus);
      for (const id of [...presenceWatch.keys()]) unwatchPresence(id);
      el.innerHTML = "";
    },
  };
}
