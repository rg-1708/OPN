import { useCallback, useEffect, useState } from "react";
import { setToken, setUnauthedHandler } from "./api.ts";
import { ToastProvider } from "./ui.tsx";
import { Login } from "./views/Login.tsx";
import { Dashboard } from "./views/Dashboard.tsx";

export function App() {
  // Session presence = the JWT's expiry (unix seconds), null when signed out.
  // Never persisted (rule 5): a reload lands back on Login. The bearer token
  // itself lives in api.ts module memory; this only gates rendering.
  const [expiresAt, setExpiresAt] = useState<number | null>(null);

  const logout = useCallback(() => {
    setToken(null);
    setExpiresAt(null);
  }, []);

  const login = useCallback((token: string, exp: number) => {
    setToken(token);
    setExpiresAt(exp);
  }, []);

  // A 401 anywhere (expired/revoked JWT) drops us to Login.
  useEffect(() => {
    setUnauthedHandler(logout);
  }, [logout]);

  // Proactive logout at token expiry, so a stale dashboard can't fire an action
  // that's guaranteed to 401. TTL is ~30 min (roadmap §Admin auth).
  useEffect(() => {
    if (expiresAt == null) return;
    const ms = expiresAt * 1000 - Date.now();
    if (ms <= 0) {
      logout();
      return;
    }
    const t = setTimeout(logout, ms);
    return () => clearTimeout(t);
  }, [expiresAt, logout]);

  return (
    <ToastProvider>
      {expiresAt != null ? <Dashboard onLogout={logout} /> : <Login onLogin={login} />}
    </ToastProvider>
  );
}
