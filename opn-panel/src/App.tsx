import { useCallback, useEffect, useState } from "react";
import { api, setToken, setUnauthedHandler } from "./api.ts";
import { ToastProvider } from "./ui.tsx";
import { Login } from "./views/Login.tsx";
import { Dashboard } from "./views/Dashboard.tsx";

export function App() {
  // Session presence = the JWT's expiry (unix seconds), null when signed out.
  // Never persisted (rule 5): a reload lands back on Login. The bearer token
  // itself lives in api.ts module memory; this only gates rendering.
  const [expiresAt, setExpiresAt] = useState<number | null>(null);

  // null = still asking Core; true = fresh deploy, show the one-time setup
  // screen; false = password already set, show login.
  const [needsSetup, setNeedsSetup] = useState<boolean | null>(null);

  // Ask once on load whether the admin password exists yet. On error, fall back
  // to the login screen so the operator can still act (login surfaces the fault).
  useEffect(() => {
    api
      .status()
      .then((s) => setNeedsSetup(!s.configured))
      .catch(() => setNeedsSetup(false));
  }, []);

  const logout = useCallback(() => {
    setToken(null);
    setExpiresAt(null);
  }, []);

  const login = useCallback((token: string, exp: number) => {
    setToken(token);
    setExpiresAt(exp);
    setNeedsSetup(false); // holding a session ⇒ the panel is configured
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
      {expiresAt != null ? (
        <Dashboard onLogout={logout} />
      ) : needsSetup == null ? null : (
        <Login onLogin={login} needsSetup={needsSetup} />
      )}
    </ToastProvider>
  );
}
