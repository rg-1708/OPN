import { useState } from "react";
import { Button, Modal } from "../ui.tsx";

// The raw API key is returned by create/rotate exactly once (cross-cutting
// rule 2) and never leaves this component's props — no store, no localStorage,
// no logging. The operator must copy it and explicitly confirm before it's gone.
export function ShowOnceKeyModal({
  apiKey,
  fingerprint,
  subject,
  onClose,
}: {
  apiKey: string;
  fingerprint: string;
  subject: string; // e.g. "acme" or "acme (rotated)"
  onClose: () => void;
}) {
  const [copied, setCopied] = useState(false);

  async function copy() {
    try {
      await navigator.clipboard.writeText(apiKey);
      setCopied(true);
    } catch {
      // Clipboard blocked (non-secure context) — select-to-copy still works.
      setCopied(false);
    }
  }

  return (
    <Modal title={`API key — ${subject}`} onClose={onClose} locked>
      <p className="text-sm text-amber-300">
        Shown once. Copy it now — it cannot be retrieved after you close this.
      </p>

      <div className="mt-4 rounded-md border border-zinc-700 bg-zinc-950 p-3">
        <code className="mono block break-all text-sm text-emerald-300 select-all">{apiKey}</code>
      </div>

      <div className="mt-2 flex items-center justify-between text-xs text-zinc-500">
        <span>
          fingerprint <span className="mono text-zinc-400">{fingerprint}</span>
        </span>
        <Button variant="ghost" onClick={copy} className="px-2 py-1 text-xs">
          {copied ? "Copied ✓" : "Copy"}
        </Button>
      </div>

      <Button variant="primary" onClick={onClose} className="mt-5 w-full">
        I saved it
      </Button>
    </Modal>
  );
}
