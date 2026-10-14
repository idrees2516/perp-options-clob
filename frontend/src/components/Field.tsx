'use client';

import type { ReactNode } from 'react';

/** Labeled form-field wrapper shared by the order ticket and admin cards. */
export function Field({
  label,
  hint,
  error,
  children,
}: {
  label: string;
  hint?: ReactNode;
  error?: string | null;
  children: ReactNode;
}) {
  return (
    <label className="flex min-w-0 flex-col gap-1">
      <span className="font-sans text-[9px] font-semibold tracking-[0.14em] text-txt-dim uppercase">
        {label}
      </span>
      {children}
      {error ? (
        <span className="text-[10px] leading-snug text-down-bright">{error}</span>
      ) : hint ? (
        <span className="text-[10px] leading-snug text-txt-dim">{hint}</span>
      ) : null}
    </label>
  );
}
