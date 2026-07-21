import type { AuditRow } from "../api.ts";

function detailText(detail: unknown): string {
  if (detail == null) return "";
  if (typeof detail === "string") return detail;
  try {
    return JSON.stringify(detail);
  } catch {
    return "";
  }
}

export function AuditLog({ rows, loading }: { rows: AuditRow[]; loading: boolean }) {
  return (
    <div className="overflow-x-auto rounded-lg border border-zinc-800">
      <table className="w-full text-left text-sm">
        <thead className="border-b border-zinc-800 text-xs tracking-wide text-zinc-500 uppercase">
          <tr>
            <th className="px-4 py-2.5 font-medium">When</th>
            <th className="px-4 py-2.5 font-medium">Action</th>
            <th className="px-4 py-2.5 font-medium">Target</th>
            <th className="px-4 py-2.5 font-medium">Detail</th>
          </tr>
        </thead>
        <tbody className="divide-y divide-zinc-800/70">
          {rows.map((r) => (
            <tr key={r.id} className="hover:bg-zinc-900/40">
              <td className="px-4 py-2.5 whitespace-nowrap text-zinc-400">
                {new Date(r.at * 1000).toLocaleString()}
              </td>
              <td className="px-4 py-2.5">
                <span className="mono rounded bg-zinc-800 px-1.5 py-0.5 text-xs text-zinc-200">
                  {r.action}
                </span>
              </td>
              <td className="mono px-4 py-2.5 text-xs text-zinc-500">{r.target_tenant ?? "—"}</td>
              {/* JSON.stringify (never innerHTML): audit detail is operator-controlled text. */}
              <td className="mono px-4 py-2.5 text-xs break-all text-zinc-400">
                {detailText(r.detail)}
              </td>
            </tr>
          ))}
          {rows.length === 0 && (
            <tr>
              <td colSpan={4} className="px-4 py-8 text-center text-zinc-500">
                {loading ? "Loading…" : "No admin actions logged yet."}
              </td>
            </tr>
          )}
        </tbody>
      </table>
    </div>
  );
}
