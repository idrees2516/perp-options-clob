'use client';

import { useEffect, useRef } from 'react';
import type { ReactNode } from 'react';

interface Props {
  open: boolean;
  title: string;
  children?: ReactNode;
  confirmLabel?: string;
  danger?: boolean;
  busy?: boolean;
  onConfirm: () => void;
  onClose: () => void;
}

/** Modal confirmation for destructive/market-sensitive admin actions. */
export function ConfirmDialog({
  open,
  title,
  children,
  confirmLabel = 'CONFIRM',
  danger = false,
  busy = false,
  onConfirm,
  onClose,
}: Props) {
  const cancelRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    if (!open) return;
    cancelRef.current?.focus();
    const onKey = (e: KeyboardEvent): void => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [open, onClose]);

  if (!open) return null;

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/70 p-4"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="confirm-dialog-title"
        className="w-full max-w-md rounded border border-edge2 bg-panel p-4 shadow-2xl shadow-black/60"
      >
        <h3 id="confirm-dialog-title" className="text-sm font-semibold">
          {title}
        </h3>
        <div className="mt-2 text-xs leading-relaxed text-txt-dim">{children}</div>
        <div className="mt-4 flex justify-end gap-2">
          <button ref={cancelRef} type="button" onClick={onClose} className="btn btn-ghost" disabled={busy}>
            CANCEL
          </button>
          <button
            type="button"
            onClick={onConfirm}
            disabled={busy}
            className={`btn ${danger ? 'btn-danger' : 'btn-accent'}`}
          >
            {busy ? 'WORKING' : confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}
