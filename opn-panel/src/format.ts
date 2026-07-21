/** Compact relative age, e.g. "3m ago", "2d ago". `null` → "never". */
export function fmtAge(unixSecs: number | null): string {
  if (unixSecs == null) return "never";
  const s = Math.max(0, Math.floor(Date.now() / 1000 - unixSecs));
  if (s < 60) return `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  return `${Math.floor(h / 24)}d ago`;
}
