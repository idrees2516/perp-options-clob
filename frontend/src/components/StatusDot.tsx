'use client';

export type DotState = 'live' | 'warn' | 'down' | 'idle';

const DOT_COLOR: Record<DotState, string> = {
  live: 'bg-up-bright',
  warn: 'bg-warn-bright',
  down: 'bg-down-bright',
  idle: 'bg-txt-dim',
};

export function StatusDot({ state, label }: { state: DotState; label?: string }) {
  return (
    <span className="inline-flex items-center gap-1.5" role="status">
      <span
        className={`h-2 w-2 rounded-full ${DOT_COLOR[state]} ${state === 'warn' || state === 'down' ? 'animate-pulse' : ''}`}
        aria-hidden
      />
      {label && (
        <span className="text-[10px] tracking-[0.14em] text-txt-dim uppercase">{label}</span>
      )}
      <span className="sr-only">{label ?? state}</span>
    </span>
  );
}

export function Badge({
  tone,
  children,
  title,
}: {
  tone: 'up' | 'down' | 'warn' | 'neutral' | 'accent';
  children: React.ReactNode;
  title?: string;
}) {
  const tones: Record<string, string> = {
    up: 'bg-up/15 text-up-bright border-up/40',
    down: 'bg-down/15 text-down-bright border-down/40',
    warn: 'bg-warn/15 text-warn-bright border-warn/40',
    neutral: 'bg-edge2/30 text-txt-dim border-edge2',
    accent: 'bg-accent/15 text-accent-bright border-accent/40',
  };
  return (
    <span
      title={title}
      className={`inline-flex items-center rounded-sm border px-1.5 py-px text-[9px] font-bold tracking-[0.1em] uppercase ${tones[tone]}`}
    >
      {children}
    </span>
  );
}
