'use client';

import type { ReactNode } from 'react';

/** Shared panel chrome: titled header row + scrollable body. */
export function Panel({
  title,
  right,
  children,
  className = '',
  bodyClassName = '',
}: {
  title: string;
  right?: ReactNode;
  children: ReactNode;
  className?: string;
  bodyClassName?: string;
}) {
  return (
    <section className={`flex min-h-0 flex-col overflow-hidden rounded border border-edge bg-panel ${className}`}>
      <header className="flex h-7 shrink-0 items-center justify-between gap-2 border-b border-edge px-2">
        <h2 className="text-[10px] font-semibold tracking-[0.14em] text-txt-dim uppercase">{title}</h2>
        <div className="flex items-center gap-2">{right}</div>
      </header>
      <div className={`min-h-0 flex-1 ${bodyClassName}`}>{children}</div>
    </section>
  );
}
