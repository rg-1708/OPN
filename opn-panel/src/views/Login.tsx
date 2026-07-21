import { useState } from "react";
import { api, errMsg } from "../api.ts";
import { Button } from "../ui.tsx";

export function Login({ onLogin }: { onLogin: (token: string, expiresAt: number) => void }) {
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const r = await api.login(password);
      setPassword(""); // don't leave the secret in a controlled input
      onLogin(r.token, r.expires_at);
    } catch (err) {
      setError(errMsg(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="flex min-h-screen items-center justify-center p-4">
      <form
        onSubmit={submit}
        className="w-full max-w-sm rounded-xl border border-zinc-800 bg-zinc-900 p-6 shadow-2xl"
      >
        <h1 className="text-lg font-semibold text-zinc-100">OPN Admin</h1>
        <p className="mt-1 text-sm text-zinc-400">Operator sign-in</p>

        <label className="mt-6 block text-sm font-medium text-zinc-300" htmlFor="pw">
          Password
        </label>
        <input
          id="pw"
          type="password"
          autoComplete="current-password"
          autoFocus
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          className="mt-1.5 w-full rounded-md border border-zinc-700 bg-zinc-950 px-3 py-2 text-sm text-zinc-100 focus:border-indigo-500 focus:outline-none focus:ring-1 focus:ring-indigo-500"
        />

        {error && (
          <p role="alert" className="mt-3 text-sm text-rose-400">
            {error}
          </p>
        )}

        <Button
          type="submit"
          variant="primary"
          disabled={busy || password.length === 0}
          className="mt-5 w-full"
        >
          {busy ? "Signing in…" : "Sign in"}
        </Button>
      </form>
    </div>
  );
}
