'use client';

import { createContext, useCallback, useContext, useMemo, useRef, useState } from 'react';
import type { ReactNode } from 'react';
import type { ToastKind, ToastPayload } from '@/lib/store';

interface ToastItem extends ToastPayload {
  id: number;
}

interface ToastContextValue {
  push(kind: ToastKind, title: string, detail?: string): void;
}

const ToastContext = createContext<ToastContextValue | null>(null);

const KIND_BORDER: Record<ToastKind, string> = {
  info: 'border-l-edge2',
  success: 'border-l-up-bright',
  warn: 'border-l-warn-bright',
  error: 'border-l-down-bright',
};
const KIND_DOT: Record<ToastKind, string> = {
  info: 'bg-txt-dim',
  success: 'bg-up-bright',
  warn: 'bg-warn-bright',
  error: 'bg-down-bright',
};

export function ToastProvider({ children }: { children: ReactNode }) {
  const [items, setItems] = useState<ToastItem[]>([]);
  const idRef = useRef(0);

  const dismiss = useCallback((id: number) => {
    setItems((prev) => prev.filter((t) => t.id !== id));
  }, []);

  const push = useCallback(
    (kind: ToastKind, title: string, detail?: string) => {
      const id = ++idRef.current;
      setItems((prev) => [...prev.slice(-4), { id, kind, title, detail }]);
      const ttl = kind === 'error' ? 8000 : 5000;
      setTimeout(() => dismiss(id), ttl);
    },
    [dismiss],
  );

  const value = useMemo(() => ({ push }), [push]);

  return (
    <ToastContext.Provider value={value}>
      {children}
      <div
        aria-live="polite"
        role="status"
        className="pointer-events-none fixed right-3 bottom-3 z-50 flex w-[22rem] max-w-[calc(100vw-1.5rem)] flex-col gap-2"
      >
        {items.map((t) => (
          <div
            key={t.id}
            className={`pointer-events-auto rounded-sm border border-edge bg-panel px-3 py-2 shadow-lg shadow-black/50 ${KIND_BORDER[t.kind]} border-l-2`}
          >
            <div className="flex items-start gap-2">
              <span className={`mt-1 h-2 w-2 shrink-0 rounded-full ${KIND_DOT[t.kind]}`} aria-hidden />
              <div className="min-w-0 flex-1">
                <p className="text-xs leading-snug font-semibold">{t.title}</p>
                {t.detail && (
                  <p className="mt-0.5 text-[11px] leading-snug break-words text-txt-dim">{t.detail}</p>
                )}
              </div>
              <button
                type="button"
                onClick={() => dismiss(t.id)}
                aria-label="Dismiss notification"
                className="rounded-sm px-1 text-xs text-txt-dim hover:bg-edge hover:text-txt"
              >
                ×
              </button>
            </div>
          </div>
        ))}
      </div>
    </ToastContext.Provider>
  );
}

export function useToast(): ToastContextValue {
  const ctx = useContext(ToastContext);
  if (!ctx) throw new Error('useToast must be used inside <ToastProvider>');
  return ctx;
}
