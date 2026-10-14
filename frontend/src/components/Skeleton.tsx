'use client';

export function Skeleton({ className = '' }: { className?: string }) {
  return <div aria-hidden className={`animate-pulse rounded-sm bg-edge2/50 ${className}`} />;
}

export function TableSkeleton({
  rows = 6,
  cols = 4,
  label = 'loading',
}: {
  rows?: number;
  cols?: number;
  label?: string;
}) {
  return (
    <div className="space-y-1 p-2" role="status" aria-label={label}>
      {Array.from({ length: rows }, (_, r) => (
        <div key={r} className="flex gap-1">
          {Array.from({ length: cols }, (_, c) => (
            <Skeleton key={c} className="h-4 flex-1" />
          ))}
        </div>
      ))}
    </div>
  );
}
