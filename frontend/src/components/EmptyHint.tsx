'use client';

/** Neutral empty-state placeholder for panels (never fake data). */
export function EmptyHint({ text }: { text: string }) {
  return (
    <div className="flex min-h-16 h-full items-center justify-center p-3">
      <p className="text-center text-[11px] tracking-wide text-txt-dim">{text}</p>
    </div>
  );
}
