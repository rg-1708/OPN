import { useCallback, useEffect, useRef, useState } from "react";
import {
  api,
  errMsg,
  type AuditRow,
  type CreatedTenant,
  type RotatedKey,
  type Stats,
  type Tenant,
} from "../api.ts";
import { Button, useToast } from "../ui.tsx";
import { StatsHeader } from "../components/StatsHeader.tsx";
import { TenantTable } from "../components/TenantTable.tsx";
import { AuditLog } from "../components/AuditLog.tsx";
import { CreateTenantModal } from "../components/CreateTenantModal.tsx";
import { ShowOnceKeyModal } from "../components/ShowOnceKeyModal.tsx";

interface RevealedKey {
  apiKey: string;
  fingerprint: string;
  subject: string;
}

export function Dashboard({ onLogout }: { onLogout: () => void }) {
  const toast = useToast();
  const [stats, setStats] = useState<Stats | null>(null);
  const [tenants, setTenants] = useState<Tenant[]>([]);
  const [audit, setAudit] = useState<AuditRow[]>([]);
  const [loading, setLoading] = useState(true);
  const [showCreate, setShowCreate] = useState(false);
  const [revealed, setRevealed] = useState<RevealedKey | null>(null);
  // Latest-wins guard: mutations fire back-to-back refreshes; if an older one
  // resolves last it must not overwrite the newer snapshot with stale rows.
  const reqSeq = useRef(0);

  const refresh = useCallback(async () => {
    const id = ++reqSeq.current;
    setLoading(true);
    try {
      const [s, t, a] = await Promise.all([api.stats(), api.tenants(), api.audit()]);
      if (id !== reqSeq.current) return; // superseded by a newer refresh
      setStats(s);
      setTenants(t);
      setAudit(a);
    } catch (err) {
      // A 401 already routed to login (api swallows it); anything else is a
      // transient/read error worth surfacing — but only from the latest refresh.
      if (id === reqSeq.current) toast("error", errMsg(err));
    } finally {
      if (id === reqSeq.current) setLoading(false);
    }
  }, [toast]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  function onCreated(t: CreatedTenant) {
    setShowCreate(false);
    setRevealed({ apiKey: t.api_key, fingerprint: t.fingerprint, subject: t.name });
    void refresh();
  }

  function onRotated(k: RotatedKey, subject: string) {
    setRevealed({ apiKey: k.api_key, fingerprint: k.fingerprint, subject });
    void refresh();
  }

  return (
    <div className="mx-auto max-w-5xl px-4 py-6">
      <header className="mb-6 flex items-center justify-between">
        <h1 className="text-lg font-semibold text-zinc-100">OPN Admin</h1>
        <div className="flex items-center gap-2">
          <Button variant="ghost" onClick={() => void refresh()} disabled={loading}>
            {loading ? "Refreshing…" : "Refresh"}
          </Button>
          <Button variant="ghost" onClick={onLogout}>
            Sign out
          </Button>
        </div>
      </header>

      <StatsHeader stats={stats} />

      <section className="mt-8">
        <div className="mb-3 flex items-center justify-between">
          <h2 className="text-sm font-semibold tracking-wide text-zinc-400 uppercase">Tenants</h2>
          <Button variant="primary" onClick={() => setShowCreate(true)}>
            + Create tenant
          </Button>
        </div>
        <TenantTable
          tenants={tenants}
          loading={loading}
          onRotated={onRotated}
          onChanged={() => void refresh()}
        />
      </section>

      <section className="mt-8">
        <h2 className="mb-3 text-sm font-semibold tracking-wide text-zinc-400 uppercase">
          Audit log
        </h2>
        <AuditLog rows={audit} loading={loading} />
      </section>

      {showCreate && (
        <CreateTenantModal onClose={() => setShowCreate(false)} onCreated={onCreated} />
      )}

      {revealed && (
        <ShowOnceKeyModal
          apiKey={revealed.apiKey}
          fingerprint={revealed.fingerprint}
          subject={revealed.subject}
          onClose={() => setRevealed(null)}
        />
      )}
    </div>
  );
}
