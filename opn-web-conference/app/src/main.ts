import "./style.css";
import { connect, type ConnectionState } from "@opn/client";
import type { CharacterInfo } from "@opn/contracts";

// The dev-auth `/join` reply (device is stripped by the sidecar — see dev-auth).
interface JoinResponse {
  token: string;
  session_id: string;
  character: CharacterInfo;
}
interface JoinError {
  code: string;
  msg: string;
}

const app = document.querySelector<HTMLDivElement>("#app")!;

/** Same-origin: Vite proxies `/join` to the dev-auth sidecar. */
async function join(name: string): Promise<JoinResponse> {
  const res = await fetch("/join", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ name }),
  });
  if (!res.ok) {
    const err = (await res.json().catch(() => null)) as JoinError | null;
    throw new Error(err?.msg ?? `join failed (${res.status})`);
  }
  return (await res.json()) as JoinResponse;
}

/** WS URL derived from the current page — Vite proxies `/ws` to Core. */
function wsUrl(): string {
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  return `${scheme}://${location.host}/ws`;
}

// Distinct label + Tailwind classes per connection state. Full class strings
// (not built up) so Tailwind v4's content scan actually emits them.
const STATE_META: Record<ConnectionState, { label: string; cls: string }> = {
  connecting: { label: "Connecting…", cls: "bg-amber-100 text-amber-800 ring-amber-300" },
  live: { label: "Live", cls: "bg-green-100 text-green-800 ring-green-300" },
  reconnecting: { label: "Reconnecting…", cls: "bg-amber-100 text-amber-800 ring-amber-300 animate-pulse" },
  taken_over: { label: "Session opened in another tab", cls: "bg-red-100 text-red-800 ring-red-300" },
  closed: { label: "Closed", cls: "bg-gray-200 text-gray-700 ring-gray-300" },
};

function escapeHtml(s: string): string {
  return s.replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!,
  );
}

interface Session {
  conn: ReturnType<typeof connect>;
  unsub: () => void;
  info: JoinResponse;
}
let session: Session | null = null;
let lastName = "";

function teardown(): void {
  if (!session) return;
  session.unsub();
  session.conn.close();
  session = null;
}

async function doJoin(name: string): Promise<void> {
  lastName = name;
  teardown(); // drop any prior connection (e.g. a taken-over one on rejoin)
  const info = await join(name);
  const conn = connect({
    url: wsUrl(),
    token: info.token,
    // Re-mint on hard auth loss. ponytail: session_id/number shown stay from the
    // first mint — refreshing the panel identity on remint is a later sprint.
    remint: async () => (await join(name)).token,
  });
  const unsub = conn.onState((s) => paintSession(info, s));
  session = { conn, unsub, info };
  paintSession(info, conn.state); // onState doesn't fire on subscribe — paint now
}

function showForm(errorMsg?: string): void {
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

function paintSession(info: JoinResponse, state: ConnectionState): void {
  const meta = STATE_META[state];
  const c = info.character;
  const takenOver = state === "taken_over";
  app.innerHTML = `
    <main class="min-h-screen grid place-items-center bg-gray-50 text-gray-900">
      <section class="flex w-96 flex-col gap-4 rounded-xl bg-white p-6 shadow">
        <div class="flex items-center justify-between gap-2">
          <h1 class="text-lg font-semibold">Session</h1>
          <span class="rounded-full px-3 py-1 text-sm font-medium ring-1 ${meta.cls}">${escapeHtml(meta.label)}</span>
        </div>
        ${takenOver
          ? `<p class="rounded-md bg-red-50 p-3 text-sm text-red-700">This session was taken over by another tab. Rejoin to continue here.</p>`
          : ""}
        <dl class="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
          <dt class="text-gray-500">Name</dt>
          <dd class="font-medium">${escapeHtml(c.framework_ref)}</dd>
          <dt class="text-gray-500">Number</dt>
          <dd>${c.number ? escapeHtml(c.number) : `<span class="text-gray-400">no number</span>`}</dd>
          <dt class="text-gray-500">Session</dt>
          <dd class="break-all font-mono text-xs">${escapeHtml(info.session_id)}</dd>
        </dl>
        <div class="flex gap-2">
          ${takenOver
            ? `<button id="rejoin-btn" class="rounded-md bg-blue-600 px-3 py-2 text-sm font-medium text-white hover:bg-blue-700">Rejoin</button>`
            : ""}
          <button id="leave-btn" class="rounded-md border border-gray-300 px-3 py-2 text-sm hover:bg-gray-100">Leave</button>
        </div>
      </section>
    </main>`;
  app.querySelector<HTMLButtonElement>("#leave-btn")!.addEventListener("click", () => {
    teardown();
    showForm();
  });
  app.querySelector<HTMLButtonElement>("#rejoin-btn")?.addEventListener("click", () => {
    doJoin(lastName).catch((err: unknown) => {
      showForm(err instanceof Error ? err.message : "rejoin failed");
    });
  });
}

showForm();
