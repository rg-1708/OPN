import type { ConnectionState } from "@opn/client";

export function escapeHtml(s: string): string {
  return s.replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!,
  );
}

// Full class strings per state (not built up) so Tailwind v4's content scan emits them.
export const STATE_META: Record<ConnectionState, { label: string; cls: string }> = {
  connecting: { label: "Connecting…", cls: "bg-amber-100 text-amber-800 ring-amber-300" },
  live: { label: "Live", cls: "bg-green-100 text-green-800 ring-green-300" },
  reconnecting: {
    label: "Reconnecting…",
    cls: "bg-amber-100 text-amber-800 ring-amber-300 animate-pulse",
  },
  taken_over: { label: "Opened in another tab", cls: "bg-red-100 text-red-800 ring-red-300" },
  closed: { label: "Closed", cls: "bg-gray-200 text-gray-700 ring-gray-300" },
};

export function stateBadge(state: ConnectionState): string {
  const m = STATE_META[state];
  return `<span class="rounded-full px-3 py-1 text-sm font-medium ring-1 ${m.cls}">${m.label}</span>`;
}
