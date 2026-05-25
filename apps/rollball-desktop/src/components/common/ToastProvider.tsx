import { useState, useEffect, useCallback, createContext, useContext, type ReactNode } from "react";
import { cn } from "../../lib/utils";

type ToastType = "success" | "error" | "warning" | "info";

interface Toast {
  id: number;
  type: ToastType;
  message: string;
  action?: { label: string; onClick: () => void };
}

interface ToastContextValue {
  addToast: (toast: Omit<Toast, "id">) => void;
}

const ToastContext = createContext<ToastContextValue>({ addToast: () => { } });

export function useToast() {
  return useContext(ToastContext);
}

let nextId = 0;

export function ToastProvider({ children }: { children: ReactNode }) {
  const [toasts, setToasts] = useState<Toast[]>([]);

  const addToast = useCallback((toast: Omit<Toast, "id">) => {
    const id = nextId++;
    setToasts((prev) => [...prev.slice(-2), { ...toast, id }]); // max 3
  }, []);

  const removeToast = useCallback((id: number) => {
    setToasts((prev) => prev.filter((t) => t.id !== id));
  }, []);

  return (
    <ToastContext.Provider value={{ addToast }}>
      {children}
      {/* Toast container */}
      <div className="fixed bottom-4 right-4 z-[100] flex flex-col gap-2">
        {toasts.map((toast) => (
          <ToastItem key={toast.id} toast={toast} onDismiss={() => removeToast(toast.id)} />
        ))}
      </div>
    </ToastContext.Provider>
  );
}

function ToastItem({ toast, onDismiss }: { toast: Toast; onDismiss: () => void }) {
  const autoDismissMs = toast.type === "error" ? 8000 : 5000;

  useEffect(() => {
    const timer = setTimeout(onDismiss, autoDismissMs);
    return () => clearTimeout(timer);
  }, [autoDismissMs, onDismiss]);

  const style: Record<ToastType, string> = {
    success: "text-green-600 dark:text-green-400",
    error: "text-red-500 dark:text-red-400",
    warning: "text-yellow-500 dark:text-yellow-400",
    info: "text-blue-500 dark:text-blue-400",
  };

  const iconMap: Record<ToastType, string> = {
    success: "",
    error: "❌",
    warning: "⚠️",
    info: "ℹ️",
  };

  return (
    <div
      className={cn(
        "flex items-start gap-2 rounded-lg border border-zinc-200 bg-white px-3 py-2.5 shadow-lg w-fit max-w-xs dark:border-zinc-700 dark:bg-zinc-800",
        style[toast.type],
      )}
      role="alert"
    >
      <span className="shrink-0 text-sm leading-5">{iconMap[toast.type]}</span>
      <p className="flex-1 text-sm leading-5 text-zinc-700 dark:text-zinc-300">{toast.message}</p>
      {toast.action && (
        <button
          onClick={toast.action.onClick}
          className="shrink-0 text-xs font-medium text-zinc-500 hover:text-zinc-700 dark:hover:text-zinc-300"
        >
          {toast.action.label}
        </button>
      )}
      <button
        onClick={onDismiss}
        className="shrink-0 text-zinc-400 hover:text-zinc-600 dark:hover:text-zinc-300"
        aria-label="Dismiss"
      >
        ✕
      </button>
    </div>
  );
}
