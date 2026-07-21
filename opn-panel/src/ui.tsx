import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";

// ---- Button -------------------------------------------------------------

type Variant = "primary" | "ghost" | "danger";

const variants: Record<Variant, string> = {
  primary:
    "bg-indigo-500 hover:bg-indigo-400 text-white disabled:bg-indigo-500/40",
  ghost: "bg-zinc-800 hover:bg-zinc-700 text-zinc-100 disabled:opacity-40",
  danger: "bg-rose-600 hover:bg-rose-500 text-white disabled:bg-rose-600/40",
};

export function Button({
  variant = "ghost",
  className = "",
  ...props
}: { variant?: Variant } & React.ButtonHTMLAttributes<HTMLButtonElement>) {
  return (
    <button
      {...props}
      className={`inline-flex items-center justify-center gap-2 rounded-md px-3 py-1.5 text-sm font-medium transition-colors focus:outline-none focus-visible:ring-2 focus-visible:ring-indigo-400 disabled:cursor-not-allowed ${variants[variant]} ${className}`}
    />
  );
}

// ---- Modal --------------------------------------------------------------

/** Centered dialog. `onClose` fires on backdrop click / Escape unless `locked`
 *  (used by the show-once key modal — it must be dismissed deliberately). */
export function Modal({
  children,
  onClose,
  locked = false,
  title,
}: {
  children: ReactNode;
  onClose: () => void;
  locked?: boolean;
  title: string;
}) {
  useEffect(() => {
    if (locked) return;
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose, locked]);

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-4"
      onClick={locked ? undefined : onClose}
      role="dialog"
      aria-modal="true"
      aria-label={title}
    >
      <div
        className="w-full max-w-md rounded-xl border border-zinc-800 bg-zinc-900 shadow-2xl"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="border-b border-zinc-800 px-5 py-3">
          <h2 className="text-sm font-semibold text-zinc-100">{title}</h2>
        </div>
        <div className="p-5">{children}</div>
      </div>
    </div>
  );
}

// ---- Toasts -------------------------------------------------------------

type Kind = "error" | "success";
interface Toast {
  id: number;
  kind: Kind;
  msg: string;
}

const ToastCtx = createContext<(kind: Kind, msg: string) => void>(() => {});

export function useToast() {
  return useContext(ToastCtx);
}

export function ToastProvider({ children }: { children: ReactNode }) {
  const [toasts, setToasts] = useState<Toast[]>([]);
  const seq = useRef(0);

  const push = useCallback((kind: Kind, msg: string) => {
    const id = ++seq.current;
    setToasts((t) => [...t, { id, kind, msg }]);
    setTimeout(() => setToasts((t) => t.filter((x) => x.id !== id)), 5000);
  }, []);

  return (
    <ToastCtx.Provider value={push}>
      {children}
      <div className="fixed bottom-4 right-4 z-60 flex w-80 flex-col gap-2">
        {toasts.map((t) => (
          <div
            key={t.id}
            role="status"
            className={`rounded-md border px-4 py-2 text-sm shadow-lg ${
              t.kind === "error"
                ? "border-rose-800 bg-rose-950 text-rose-200"
                : "border-emerald-800 bg-emerald-950 text-emerald-200"
            }`}
          >
            {t.msg}
          </div>
        ))}
      </div>
    </ToastCtx.Provider>
  );
}
