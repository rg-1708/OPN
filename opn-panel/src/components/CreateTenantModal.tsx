import { useState } from "react";
import { api, errMsg, type CreatedTenant } from "../api.ts";
import { Button, Modal } from "../ui.tsx";

export function CreateTenantModal({
  onClose,
  onCreated,
}: {
  onClose: () => void;
  onCreated: (t: CreatedTenant) => void; // parent shows the show-once key + refetches
}) {
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const trimmed = name.trim();
  const valid = trimmed.length >= 1 && trimmed.length <= 128;

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (!valid) return;
    setBusy(true);
    setError(null);
    try {
      const created = await api.createTenant(trimmed);
      onCreated(created);
    } catch (err) {
      setError(errMsg(err));
      setBusy(false); // stay open so the operator can retry
    }
  }

  return (
    <Modal title="Create tenant" onClose={onClose}>
      <form onSubmit={submit}>
        <label className="block text-sm font-medium text-zinc-300" htmlFor="tname">
          Name
        </label>
        <input
          id="tname"
          autoFocus
          value={name}
          maxLength={128}
          onChange={(e) => setName(e.target.value)}
          placeholder="acme-corp"
          className="mt-1.5 w-full rounded-md border border-zinc-700 bg-zinc-950 px-3 py-2 text-sm text-zinc-100 focus:border-indigo-500 focus:outline-none focus:ring-1 focus:ring-indigo-500"
        />
        <p className="mt-1.5 text-xs text-zinc-500">
          Creates a fresh world + tenant. The API key is shown once on the next screen.
        </p>

        {error && (
          <p role="alert" className="mt-3 text-sm text-rose-400">
            {error}
          </p>
        )}

        <div className="mt-5 flex justify-end gap-2">
          <Button type="button" variant="ghost" onClick={onClose} disabled={busy}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" disabled={busy || !valid}>
            {busy ? "Creating…" : "Create"}
          </Button>
        </div>
      </form>
    </Modal>
  );
}
