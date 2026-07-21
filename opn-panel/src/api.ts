// Typed client for Core's admin API (opn-panel-roadmap.md §Admin API surface).
// The JWT lives ONLY in module memory (cross-cutting rule 5 — no secrets at
// rest); a reload drops it and forces re-login. Same-origin fetch: dev via the
// Vite proxy, prod via the admin bind serving this SPA.

export interface LoginResp {
  token: string;
  expires_at: number; // unix seconds
}

export interface Tenant {
  id: string;
  name: string;
  created_at: number; // unix seconds
  fingerprint: string;
  frozen: boolean;
  last_session: number | null; // unix seconds
  allowed_origins: string[]; // browser origins for the WS Origin gate
}

// create + rotate return the raw key EXACTLY ONCE (cross-cutting rule 2).
export interface CreatedTenant {
  id: string;
  name: string;
  fingerprint: string;
  api_key: string;
}

export interface RotatedKey {
  id: string;
  fingerprint: string;
  api_key: string;
}

export interface Stats {
  tenants: number;
  live_sessions: number;
  active_calls: number;
  messages_24h: number;
}

export interface AuditRow {
  id: number;
  at: number; // unix seconds
  action: string;
  target_tenant: string | null;
  detail: unknown | null;
}

let token: string | null = null;
let onUnauthed: (() => void) | null = null;

export function setToken(t: string | null): void {
  token = t;
}

/** Called when a request 401s with a token set — the session expired/was revoked. */
export function setUnauthedHandler(fn: () => void): void {
  onUnauthed = fn;
}

async function call<T>(method: string, path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = {};
  if (body !== undefined) headers["content-type"] = "application/json";
  if (token) headers["authorization"] = `Bearer ${token}`;

  // Assign body only when present — `exactOptionalPropertyTypes` rejects an
  // explicit `undefined` for RequestInit.body.
  const init: RequestInit = { method, headers };
  if (body !== undefined) init.body = JSON.stringify(body);

  const res = await fetch(`/admin/v1${path}`, init);

  // An expired/revoked admin JWT is fatal to the session — bounce to login and
  // swallow the response so the caller (a now-unmounting view) doesn't ALSO
  // surface an "unauthorized" toast. Guard on `token` so the login POST itself
  // (no token) still reports a wrong password normally.
  // ponytail: returns a never-resolving promise; the pending caller is torn down
  // by logout and GC'd. Fine here; revisit if a caller must survive a 401.
  if (res.status === 401 && token) {
    onUnauthed?.();
    return new Promise<never>(() => {});
  }

  if (!res.ok) {
    let msg = res.statusText || "request failed";
    try {
      const j = (await res.json()) as { msg?: string };
      if (j.msg) msg = j.msg;
    } catch {
      // non-JSON error body (e.g. a proxy 502) — keep the status text.
    }
    throw new Error(msg);
  }

  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

export const api = {
  // Unauthed. `configured: false` on a fresh deploy → show the one-time setup
  // screen; `true` → show login. See Core admin.rs status/setup.
  status: () => call<{ configured: boolean }>("GET", "/status"),
  // First-launch only: sets the admin password and returns a JWT (auto-login).
  // 409s once a password exists.
  setup: (password: string) => call<LoginResp>("POST", "/setup", { password }),
  login: (password: string) => call<LoginResp>("POST", "/login", { password }),
  tenants: () => call<Tenant[]>("GET", "/tenants"),
  createTenant: (name: string) => call<CreatedTenant>("POST", "/tenants", { name }),
  rotateKey: (id: string) => call<RotatedKey>("POST", `/tenants/${id}/rotate-key`),
  deleteTenant: (id: string) => call<{ id: string; deleted: boolean }>("DELETE", `/tenants/${id}`),
  setOrigins: (id: string, origins: string[]) =>
    call<{ id: string; allowed_origins: string[] }>("PUT", `/tenants/${id}/origins`, { origins }),
  freeze: (id: string) => call<{ id: string; frozen: boolean }>("POST", `/tenants/${id}/freeze`),
  unfreeze: (id: string) => call<{ id: string; frozen: boolean }>("POST", `/tenants/${id}/unfreeze`),
  stats: () => call<Stats>("GET", "/stats"),
  audit: (limit = 100) => call<AuditRow[]>("GET", `/audit?limit=${limit}`),
};

/** Human message for any thrown error, without leaking undefined. */
export function errMsg(e: unknown): string {
  return e instanceof Error ? e.message : "unexpected error";
}
