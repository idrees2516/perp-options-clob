'use client';

/**
 * Hand-rolled SVG sparkline (no chart libraries).
 * Values are rendered normalized to the viewport; the last sample gets an
 * endpoint dot. Callers maintain the ring buffer.
 */
export function Sparkline({
  values,
  width = 280,
  height = 44,
  className = '',
}: {
  values: number[];
  width?: number;
  height?: number;
  className?: string;
}) {
  if (values.length < 2) {
    return (
      <div
        className="flex items-center justify-center text-[10px] text-txt-dim"
        style={{ width, height }}
      >
        collecting samples
      </div>
    );
  }
  const min = Math.min(...values);
  const max = Math.max(...values);
  const span = max - min || 1;
  const n = values.length;
  const points = values
    .map((v, i) => {
      const x = 1 + (i / (n - 1)) * (width - 3);
      const y = height - 2 - ((v - min) / span) * (height - 4);
      return `${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .join(' ');
  const last = values[n - 1]!;
  const lastY = height - 2 - ((last - min) / span) * (height - 4);
  return (
    <svg
      width={width}
      height={height}
      viewBox={`0 0 ${width} ${height}`}
      role="img"
      aria-label={`sparkline: current ${last}, min ${min}, max ${max}`}
      className={className}
    >
      <polyline points={points} fill="none" stroke="currentColor" strokeWidth="1.5" />
      <circle cx={width - 2} cy={lastY} r="2" fill="currentColor" />
    </svg>
  );
}

/** Fixed-size numeric ring buffer for poll-driven series. */
export class RingBuffer {
  private readonly buf: number[];
  private idx = 0;
  private count = 0;

  constructor(readonly capacity: number) {
    this.buf = new Array<number>(capacity).fill(0);
  }

  push(v: number): void {
    this.buf[this.idx] = v;
    this.idx = (this.idx + 1) % this.capacity;
    this.count = Math.min(this.count + 1, this.capacity);
  }

  /** oldest -> newest */
  values(): number[] {
    if (this.count < this.capacity) return this.buf.slice(0, this.count);
    return [...this.buf.slice(this.idx), ...this.buf.slice(0, this.idx)];
  }
}
