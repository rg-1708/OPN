import {
  createCallManager,
  type CallEndReason,
  type CallManager,
  type CallView,
  type OpnConnection,
} from "@opn/client";
import { escapeHtml } from "./ui.ts";

// 1:1 call overlay (roadmap W2). One CallManager per session drives every phase
// from Core `calls.state` snapshots; this module only renders the view and wires
// the accept/decline/hangup buttons. Video/audio elements are built once per
// phase and their `srcObject` is patched on stream changes so playback never
// restarts mid-call.

export interface CallController {
  /** Dial a room member. `label` is the callee's display name for the overlay. */
  startCall: (calleeNumber: string, video: boolean, label: string) => void;
  /** Wire an incoming `ring` notify (call_id from its payload) to the manager. */
  handleRing: (callId: string, label?: string) => void;
  dispose: () => void;
}

const END_TEXT: Record<CallEndReason, string> = {
  busy: "Busy — they're already on a call.",
  declined: "Call declined.",
  no_answer: "No answer.",
  ended: "Call ended.",
  failed: "Call failed.",
};

/** Best-effort OS notification for an incoming ring (roadmap W2: "browser notification + in-app modal"). */
function osNotify(title: string): void {
  try {
    if (typeof Notification !== "undefined" && Notification.permission === "granted") {
      new Notification(title);
    }
  } catch {
    /* notifications unavailable / blocked — the in-app modal still shows */
  }
}

export function mountCalls(conn: OpnConnection, selfId: string): CallController {
  const host = document.createElement("div");
  host.id = "call-overlay";
  document.body.appendChild(host);

  // Peer label for the current call (callee name when we dial; caller info from
  // the ring when we receive). Not part of CallView — it's app-side directory data.
  let label = "";
  let mountedPhase: CallView["phase"] | null = null;

  if (typeof Notification !== "undefined" && Notification.permission === "default") {
    void Notification.requestPermission().catch(() => {});
  }

  const manager: CallManager = createCallManager(conn, { selfId, onChange: render });

  function render(view: CallView): void {
    if (view.phase === "idle") {
      host.innerHTML = "";
      mountedPhase = null;
      return;
    }
    if (view.phase !== mountedPhase) {
      host.innerHTML = shell(view);
      wire(view);
      mountedPhase = view.phase;
    }
    const peerEl = host.querySelector<HTMLElement>("#call-peer");
    if (peerEl) peerEl.textContent = label || view.peer || "Unknown";
    // Kind can arrive after the first render (from the snapshot-on-sub), so patch
    // it live rather than baking it into the once-per-phase shell.
    const kindEl = host.querySelector<HTMLElement>("#call-kind");
    if (kindEl) kindEl.textContent = view.kind === "video" ? "video call" : "voice call";
    if (view.phase === "active") {
      patchStream("#remote-video", view.remoteStream);
      patchStream("#local-video", view.localStream);
    }
  }

  function patchStream(sel: string, stream: MediaStream | null): void {
    const el = host.querySelector<HTMLVideoElement>(sel);
    if (el && el.srcObject !== stream) el.srcObject = stream;
  }

  function shell(view: CallView): string {
    const kindLabel = view.kind === "video" ? "Video call" : "Voice call";
    if (view.phase === "ringing") {
      return card(`
        <p class="text-sm text-gray-500">Incoming <span id="call-kind">call</span></p>
        <p class="text-lg font-semibold" id="call-peer"></p>
        <div class="mt-2 flex gap-2">
          <button id="call-accept" class="flex-1 rounded-md bg-green-600 px-4 py-2 font-medium text-white hover:bg-green-700">Accept</button>
          <button id="call-decline" class="flex-1 rounded-md bg-red-600 px-4 py-2 font-medium text-white hover:bg-red-700">Decline</button>
        </div>`);
    }
    if (view.phase === "calling") {
      return card(`
        <p class="text-sm text-gray-500">Calling…</p>
        <p class="text-lg font-semibold" id="call-peer"></p>
        <button id="call-hangup" class="mt-2 w-full rounded-md bg-red-600 px-4 py-2 font-medium text-white hover:bg-red-700">Cancel</button>`);
    }
    if (view.phase === "active") {
      return `
        <div class="fixed inset-0 z-50 flex flex-col bg-black/90">
          <div class="flex items-center gap-3 px-4 py-2 text-white">
            <span class="font-semibold" id="call-peer"></span>
            <span class="text-xs text-gray-300">${escapeHtml(kindLabel)}</span>
          </div>
          <div class="relative flex-1">
            <video id="remote-video" autoplay playsinline class="h-full w-full bg-black object-contain"></video>
            <video id="local-video" autoplay playsinline muted
              class="absolute bottom-4 right-4 w-40 rounded-lg border border-white/20 bg-black object-cover shadow-lg"></video>
          </div>
          <div class="flex justify-center py-4">
            <button id="call-hangup" class="rounded-full bg-red-600 px-6 py-3 font-medium text-white hover:bg-red-700">Hang up</button>
          </div>
        </div>`;
    }
    // ended
    return card(`
      <p class="text-lg font-semibold" id="call-peer"></p>
      <p class="text-sm text-gray-600">${escapeHtml(view.endReason ? END_TEXT[view.endReason] : "Call ended.")}</p>
      <button id="call-clear" class="mt-2 w-full rounded-md bg-gray-800 px-4 py-2 font-medium text-white hover:bg-gray-900">Close</button>`);
  }

  function card(inner: string): string {
    return `
      <div class="fixed inset-0 z-50 grid place-items-center bg-black/40">
        <div class="w-72 rounded-xl bg-white p-5 shadow-xl">${inner}</div>
      </div>`;
  }

  function wire(view: CallView): void {
    host.querySelector<HTMLButtonElement>("#call-accept")?.addEventListener("click", () => void manager.accept());
    host.querySelector<HTMLButtonElement>("#call-decline")?.addEventListener("click", () => manager.decline());
    host.querySelector<HTMLButtonElement>("#call-hangup")?.addEventListener("click", () => manager.hangup());
    host.querySelector<HTMLButtonElement>("#call-clear")?.addEventListener("click", () => manager.clear());
    void view;
  }

  return {
    startCall(calleeNumber, video, lbl) {
      label = lbl;
      void manager.start(calleeNumber, video);
    },
    handleRing(callId, lbl) {
      label = lbl ?? "Incoming call";
      osNotify(`Incoming call — ${label}`);
      manager.onRing(callId);
    },
    dispose() {
      manager.dispose();
      host.remove();
      mountedPhase = null;
    },
  };
}
