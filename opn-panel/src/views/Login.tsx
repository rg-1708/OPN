import { useState } from "react";
import { api, errMsg } from "../api.ts";
import { Button } from "../ui.tsx";

const MIN_PASSWORD_LEN = 12; // must match Core admin.rs MIN_PASSWORD_LEN

// One screen, two modes: `needsSetup` (fresh deploy — create the operator
// password, confirm it, then auto-login) vs normal sign-in. Core enforces the
// same one-shot rule server-side; the confirm field + length check here are
// just fast feedback.
export function Login({
  onLogin,
  needsSetup,
}: {
  onLogin: (token: string, expiresAt: number) => void;
  needsSetup: boolean;
}) {
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const tooShort = needsSetup && password.length > 0 && password.length < MIN_PASSWORD_LEN;
  const mismatch = needsSetup && confirm.length > 0 && confirm !== password;
  const canSubmit =
    password.length > 0 &&
    (!needsSetup || (password.length >= MIN_PASSWORD_LEN && confirm === password));

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (!canSubmit) return;
    setBusy(true);
    setError(null);
    try {
      const r = needsSetup ? await api.setup(password) : await api.login(password);
      setPassword(""); // don't leave the secret in a controlled input
      setConfirm("");
      onLogin(r.token, r.expires_at);
    } catch (err) {
      setError(errMsg(err));
    } finally {
      setBusy(false);
    }
  }

  const inputCls =
    "mt-1.5 w-full rounded-md border border-zinc-700 bg-zinc-950 px-3 py-2 text-sm text-zinc-100 focus:border-indigo-500 focus:outline-none focus:ring-1 focus:ring-indigo-500";

  return (
    <div className="flex min-h-screen items-center justify-center p-4">
      <form
        onSubmit={submit}
        className="w-full max-w-sm rounded-xl border border-zinc-800 bg-zinc-900 p-6 shadow-2xl"
      >
        <h1 className="text-lg font-semibold text-zinc-100">OPN Admin</h1>
        <p className="mt-1 text-sm text-zinc-400">
          {needsSetup ? "First launch — set the operator password" : "Operator sign-in"}
        </p>

        <label className="mt-6 block text-sm font-medium text-zinc-300" htmlFor="pw">
          {needsSetup ? "New password" : "Password"}
        </label>
        <input
          id="pw"
          type="password"
          autoComplete={needsSetup ? "new-password" : "current-password"}
          autoFocus
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          className={inputCls}
        />
        {needsSetup && (
          <p className="mt-1 text-xs text-zinc-500">At least {MIN_PASSWORD_LEN} characters.</p>
        )}

        {needsSetup && (
          <>
            <label className="mt-4 block text-sm font-medium text-zinc-300" htmlFor="pw2">
              Confirm password
            </label>
            <input
              id="pw2"
              type="password"
              autoComplete="new-password"
              value={confirm}
              onChange={(e) => setConfirm(e.target.value)}
              className={inputCls}
            />
          </>
        )}

        {(tooShort || mismatch || error) && (
          <p role="alert" className="mt-3 text-sm text-rose-400">
            {error ?? (tooShort ? `At least ${MIN_PASSWORD_LEN} characters.` : "Passwords don't match.")}
          </p>
        )}

        <Button
          type="submit"
          variant="primary"
          disabled={busy || !canSubmit}
          className="mt-5 w-full"
        >
          {busy
            ? needsSetup
              ? "Setting…"
              : "Signing in…"
            : needsSetup
              ? "Set password"
              : "Sign in"}
        </Button>
      </form>
    </div>
  );
}
