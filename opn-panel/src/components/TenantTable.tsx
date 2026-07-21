import { useState } from "react";
import { api, errMsg, type RotatedKey, type Tenant } from "../api.ts";
import { fmtAge } from "../format.ts";
import { Button, Modal, useToast } from "../ui.tsx";

export function TenantTable({
  tenants,
  loading,
  onRotated,
  onChanged,
}: {
  tenants: Tenant[];
  loading: boolean;
  onRotated: (key: RotatedKey, subject: string) => void; // parent shows show-once modal
  onChanged: () => void; // refetch after freeze/unfreeze
}) {
  const toast = useToast();
  const [busy, setBusy] = useState<Set<string>>(new Set());
  const [confirmRotate, setConfirmRotate] = useState<Tenant | null>(null);
  const [confirmDelete, setConfirmDelete] = useState<Tenant | null>(null);
  const [editOrigins, setEditOrigins] = useState<Tenant | null>(null);
  const [originsDraft, setOriginsDraft] = useState("");

  function mark(id: string, on: boolean) {
    setBusy((s) => {
      const next = new Set(s);
      if (on) next.add(id);
      else next.delete(id);
      return next;
    });
  }

  async function toggleFreeze(t: Tenant) {
    mark(t.id, true);
    try {
      if (t.frozen) await api.unfreeze(t.id);
      else await api.freeze(t.id);
      toast("success", `${t.name} ${t.frozen ? "unfrozen" : "frozen"}`);
      onChanged();
    } catch (err) {
      toast("error", errMsg(err));
    } finally {
      mark(t.id, false);
    }
  }

  async function doRotate(t: Tenant) {
    setConfirmRotate(null);
    mark(t.id, true);
    try {
      const key = await api.rotateKey(t.id);
      onRotated(key, `${t.name} (rotated)`);
    } catch (err) {
      toast("error", errMsg(err));
    } finally {
      mark(t.id, false);
    }
  }

  async function doSaveOrigins(t: Tenant) {
    const origins = originsDraft
      .split("\n")
      .map((s) => s.trim())
      .filter(Boolean);
    setEditOrigins(null);
    mark(t.id, true);
    try {
      await api.setOrigins(t.id, origins);
      toast("success", `${t.name} origins saved — live within ~60 s`);
      onChanged();
    } catch (err) {
      toast("error", errMsg(err));
    } finally {
      mark(t.id, false);
    }
  }

  async function doDelete(t: Tenant) {
    setConfirmDelete(null);
    mark(t.id, true);
    try {
      await api.deleteTenant(t.id);
      toast("success", `${t.name} deleted`);
      onChanged();
    } catch (err) {
      // 409 → "tenant has live sessions; freeze it and let them expire first"
      toast("error", errMsg(err));
    } finally {
      mark(t.id, false);
    }
  }

  return (
    <>
      <div className="overflow-x-auto rounded-lg border border-zinc-800">
        <table className="w-full text-left text-sm">
          <thead className="border-b border-zinc-800 text-xs tracking-wide text-zinc-500 uppercase">
            <tr>
              <th className="px-4 py-2.5 font-medium">Name</th>
              <th className="px-4 py-2.5 font-medium">Status</th>
              <th className="px-4 py-2.5 font-medium">Key</th>
              <th className="px-4 py-2.5 font-medium">Last session</th>
              <th className="px-4 py-2.5 text-right font-medium">Actions</th>
            </tr>
          </thead>
          <tbody className="divide-y divide-zinc-800/70">
            {tenants.map((t) => {
              const rowBusy = busy.has(t.id);
              return (
                <tr key={t.id} className="hover:bg-zinc-900/40">
                  <td className="px-4 py-2.5">
                    <div className="font-medium text-zinc-100">{t.name}</div>
                    <div className="mono text-xs text-zinc-600">{t.id}</div>
                  </td>
                  <td className="px-4 py-2.5">
                    {t.frozen ? (
                      <span className="rounded-full bg-rose-950 px-2 py-0.5 text-xs font-medium text-rose-300">
                        frozen
                      </span>
                    ) : (
                      <span className="rounded-full bg-emerald-950 px-2 py-0.5 text-xs font-medium text-emerald-300">
                        active
                      </span>
                    )}
                  </td>
                  <td className="mono px-4 py-2.5 text-xs text-zinc-400">{t.fingerprint}</td>
                  <td className="px-4 py-2.5 text-zinc-400">{fmtAge(t.last_session)}</td>
                  <td className="px-4 py-2.5">
                    <div className="flex justify-end gap-2">
                      <Button
                        variant="ghost"
                        disabled={rowBusy}
                        onClick={() => {
                          setOriginsDraft(t.allowed_origins.join("\n"));
                          setEditOrigins(t);
                        }}
                        className="px-2 py-1 text-xs"
                      >
                        Origins{t.allowed_origins.length > 0 ? ` (${t.allowed_origins.length})` : ""}
                      </Button>
                      <Button
                        variant="ghost"
                        disabled={rowBusy}
                        onClick={() => setConfirmRotate(t)}
                        className="px-2 py-1 text-xs"
                      >
                        Rotate key
                      </Button>
                      <Button
                        variant={t.frozen ? "ghost" : "danger"}
                        disabled={rowBusy}
                        onClick={() => toggleFreeze(t)}
                        className="px-2 py-1 text-xs"
                      >
                        {t.frozen ? "Unfreeze" : "Freeze"}
                      </Button>
                      <Button
                        variant="danger"
                        disabled={rowBusy}
                        onClick={() => setConfirmDelete(t)}
                        className="px-2 py-1 text-xs"
                      >
                        Delete
                      </Button>
                    </div>
                  </td>
                </tr>
              );
            })}
            {tenants.length === 0 && (
              <tr>
                <td colSpan={5} className="px-4 py-8 text-center text-zinc-500">
                  {loading ? "Loading…" : "No tenants yet."}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>

      {confirmRotate && (
        <Modal title="Rotate API key" onClose={() => setConfirmRotate(null)}>
          <p className="text-sm text-zinc-300">
            Rotating <span className="font-medium text-zinc-100">{confirmRotate.name}</span> issues a
            new key and <span className="text-amber-300">invalidates the current one immediately</span>.
            Clients using the old key stop authenticating at once.
          </p>
          <div className="mt-5 flex justify-end gap-2">
            <Button variant="ghost" onClick={() => setConfirmRotate(null)}>
              Cancel
            </Button>
            <Button variant="danger" onClick={() => doRotate(confirmRotate)}>
              Rotate key
            </Button>
          </div>
        </Modal>
      )}

      {editOrigins && (
        <Modal title="Allowed origins" onClose={() => setEditOrigins(null)}>
          <p className="text-sm text-zinc-300">
            Browser origins allowed to open a WebSocket for{" "}
            <span className="font-medium text-zinc-100">{editOrigins.name}</span>, one per line, as{" "}
            <span className="mono text-zinc-400">scheme://host[:port]</span> — no path. FiveM NUI
            origins are always allowed and don't belong here.
          </p>
          <textarea
            value={originsDraft}
            onChange={(e) => setOriginsDraft(e.target.value)}
            rows={5}
            spellCheck={false}
            placeholder={"https://app.example.com\nhttp://localhost:5173"}
            className="mono mt-3 w-full rounded-md border border-zinc-700 bg-zinc-900 px-3 py-2 text-xs text-zinc-200 placeholder:text-zinc-600 focus:border-zinc-500 focus:outline-none"
          />
          <p className="mt-2 text-xs text-zinc-500">
            Replaces the whole list. New connections pick it up within ~60 s; live sockets are
            unaffected.
          </p>
          <div className="mt-5 flex justify-end gap-2">
            <Button variant="ghost" onClick={() => setEditOrigins(null)}>
              Cancel
            </Button>
            <Button variant="primary" onClick={() => doSaveOrigins(editOrigins)}>
              Save origins
            </Button>
          </div>
        </Modal>
      )}

      {confirmDelete && (
        <Modal title="Delete tenant" onClose={() => setConfirmDelete(null)}>
          <p className="text-sm text-zinc-300">
            Permanently delete <span className="font-medium text-zinc-100">{confirmDelete.name}</span>{" "}
            and its API key. <span className="text-rose-300">This cannot be undone</span> — the key is
            gone and the client must be re-issued as a new tenant.
          </p>
          <p className="mt-2 text-xs text-zinc-500">
            Refused if the tenant has live sessions — freeze it and let them expire first.
          </p>
          <div className="mt-5 flex justify-end gap-2">
            <Button variant="ghost" onClick={() => setConfirmDelete(null)}>
              Cancel
            </Button>
            <Button variant="danger" onClick={() => doDelete(confirmDelete)}>
              Delete tenant
            </Button>
          </div>
        </Modal>
      )}
    </>
  );
}
