import type { Stats } from "../api.ts";

const TILES: { key: keyof Stats; label: string }[] = [
  { key: "tenants", label: "Tenants" },
  { key: "live_sessions", label: "Live sessions" },
  { key: "active_calls", label: "Active calls" },
  { key: "messages_24h", label: "Messages / 24h" },
];

export function StatsHeader({ stats }: { stats: Stats | null }) {
  return (
    <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
      {TILES.map((t) => (
        <div key={t.key} className="rounded-lg border border-zinc-800 bg-zinc-900 px-4 py-3">
          <div className="text-xs font-medium tracking-wide text-zinc-500 uppercase">{t.label}</div>
          <div className="mono mt-1 text-2xl font-semibold text-zinc-100 tabular-nums">
            {stats ? stats[t.key].toLocaleString() : "—"}
          </div>
        </div>
      ))}
    </div>
  );
}
