"use client";

/**
 * Price chart — canvas rendering of the trade tape aggregated into candles.
 * Auto-scales, gradient area fill, last-price marker, crosshair on hover.
 */

import { memo, useEffect, useMemo, useRef, useState } from "react";
import { useVenueStore } from "@/lib/venue-store";
import { usd } from "@/lib/fmt";

type Bucket = { ts: number; open: number; high: number; low: number; close: number; volume: number };

const BUCKET_MS = 4_000;
const MAX_BUCKETS = 240;

export const PriceChart = memo(function PriceChart() {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const wrapRef = useRef<HTMLDivElement | null>(null);
  const prints = useVenueStore((s) => s.prints);
  const activeSymbol = useVenueStore((s) => s.activeSymbol);
  const book = useVenueStore((s) => s.books[s.activeSymbol]);
  const [hover, setHover] = useState<{ x: number; y: number } | null>(null);
  const [size, setSize] = useState({ w: 0, h: 0 });

  useEffect(() => {
    const el = wrapRef.current;
    if (!el) return;
    const ro = new ResizeObserver((entries) => {
      const r = entries[0]!.contentRect;
      setSize({ w: r.width, h: r.height });
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const buckets = useMemo(() => {
    const out: Bucket[] = [];
    const seen = new Map<number, Bucket>();
    // prints arrive oldest → newest; filter by symbol
    for (const p of prints) {
      if (p.symbol !== activeSymbol) continue;
      const price = Number(p.price_ticks);
      const bkt = Math.floor(p.ts / BUCKET_MS) * BUCKET_MS;
      let b = seen.get(bkt);
      if (!b) {
        b = { ts: bkt, open: price, high: price, low: price, close: price, volume: Number(p.qty_lots) };
        seen.set(bkt, b);
        out.push(b);
      } else {
        b.high = Math.max(b.high, price);
        b.low = Math.min(b.low, price);
        b.close = price;
        b.volume += Number(p.qty_lots);
      }
    }
    return out.slice(-MAX_BUCKETS);
  }, [prints, activeSymbol]);

  const last = buckets.length ? buckets[buckets.length - 1]!.close : null;

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas || !size.w || !size.h) return;
    const dpr = Math.min(2, window.devicePixelRatio || 1);
    canvas.width = size.w * dpr;
    canvas.height = size.h * dpr;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    ctx.scale(dpr, dpr);
    drawChart(ctx, size.w, size.h, buckets, hover, last);
  }, [size, buckets, hover, last]);

  const first = buckets[0]?.open ?? null;

  return (
    <div
      ref={wrapRef}
      className="relative h-full w-full overflow-hidden"
      onMouseMove={(e) => {
        const r = e.currentTarget.getBoundingClientRect();
        setHover({ x: e.clientX - r.left, y: e.clientY - r.top });
      }}
      onMouseLeave={() => setHover(null)}
    >
      <canvas ref={canvasRef} className="absolute inset-0 w-full h-full" aria-label="Price chart" />
      <div className="absolute top-2 left-3 flex items-center gap-2 pointer-events-none">
        <span className="text-[9.5px] uppercase tracking-wider text-muted-foreground/60">{activeSymbol}</span>
        <span className="text-[9.5px] text-muted-foreground/50 font-mono">{(BUCKET_MS / 1000).toFixed(0)}s</span>
        {first != null && last != null && (
          <span className={`num text-[10px] ${last >= first ? "text-up" : "text-down"}`}>
            {(((last - first) / first) * 100).toFixed(2)}%
          </span>
        )}
      </div>
      <div className="absolute bottom-2 left-3 text-[9px] text-muted-foreground/40 font-mono pointer-events-none">
        {buckets.length} candles · canvas-rendered · tape-aggregated
      </div>
    </div>
  );
});

function drawChart(
  ctx: CanvasRenderingContext2D,
  w: number,
  h: number,
  buckets: Bucket[],
  hover: { x: number; y: number } | null,
  last: number | null,
) {
  const css = getComputedStyle(document.documentElement);
  const up = css.getPropertyValue("--up").trim() || "#22c55e";
  const down = css.getPropertyValue("--down").trim() || "#ef4444";
  const muted = css.getPropertyValue("--muted-foreground").trim() || "#888";
  const primary = css.getPropertyValue("--primary").trim() || "#2dd4bf";
  const hairline = css.getPropertyValue("--hairline").trim() || "#222";

  ctx.clearRect(0, 0, w, h);

  const padR = 62;
  const padB = 18;
  const padT = 8;
  const plotW = w - padR - 8;
  const plotH = h - padB - padT;

  if (buckets.length < 2) {
    ctx.fillStyle = muted;
    ctx.font = "11px ui-monospace, monospace";
    ctx.fillText("awaiting prints…", 12, h / 2);
    return;
  }

  let min = Infinity;
  let max = -Infinity;
  for (const b of buckets) {
    min = Math.min(min, b.low);
    max = Math.max(max, b.high);
  }
  const range = max - min || 1;
  min -= range * 0.08;
  max += range * 0.08;
  const y = (v: number) => padT + plotH - ((v - min) / (max - min)) * plotH;
  const x = (i: number) => 8 + (i / (buckets.length - 1)) * plotW;

  // Grid lines + price scale
  ctx.strokeStyle = hairline;
  ctx.globalAlpha = 0.35;
  ctx.lineWidth = 1;
  ctx.font = "9.5px ui-monospace, monospace";
  for (let i = 0; i <= 4; i++) {
    const v = min + ((max - min) * i) / 4;
    const yy = y(v);
    ctx.beginPath();
    ctx.moveTo(8, yy);
    ctx.lineTo(8 + plotW, yy);
    ctx.stroke();
    ctx.globalAlpha = 0.75;
    ctx.fillStyle = muted;
    ctx.fillText(v.toLocaleString("en-US"), 8 + plotW + 6, yy + 3);
    ctx.globalAlpha = 0.35;
  }
  ctx.globalAlpha = 1;

  const rising = buckets[buckets.length - 1]!.close >= buckets[0]!.open;
  const lineColor = rising ? up : down;

  // Area fill
  const grad = ctx.createLinearGradient(0, padT, 0, h - padB);
  grad.addColorStop(0, withAlpha(lineColor, 0.22));
  grad.addColorStop(1, withAlpha(lineColor, 0.0));
  ctx.beginPath();
  ctx.moveTo(x(0), y(buckets[0]!.close));
  buckets.forEach((b, i) => ctx.lineTo(x(i), y(b.close)));
  ctx.lineTo(x(buckets.length - 1), h - padB);
  ctx.lineTo(x(0), h - padB);
  ctx.closePath();
  ctx.fillStyle = grad;
  ctx.fill();

  // Line
  ctx.beginPath();
  ctx.moveTo(x(0), y(buckets[0]!.close));
  buckets.forEach((b, i) => ctx.lineTo(x(i), y(b.close)));
  ctx.strokeStyle = lineColor;
  ctx.lineWidth = 1.6;
  ctx.lineJoin = "round";
  ctx.stroke();

  // Last price marker
  if (last != null) {
    const ly = y(last);
    ctx.setLineDash([4, 4]);
    ctx.strokeStyle = withAlpha(primary, 0.7);
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(8, ly);
    ctx.lineTo(8 + plotW, ly);
    ctx.stroke();
    ctx.setLineDash([]);
    // price pill
    ctx.fillStyle = primary;
    const label = last.toLocaleString("en-US");
    ctx.font = "bold 10px ui-monospace, monospace";
    const tw = ctx.measureText(label).width + 10;
    roundRect(ctx, 8 + plotW + 2, ly - 8, tw, 16, 4);
    ctx.fill();
    ctx.fillStyle = "#0a0e14";
    ctx.fillText(label, 8 + plotW + 7, ly + 3.5);
  }

  // Crosshair
  if (hover && hover.x > 8 && hover.x < 8 + plotW && hover.y > padT && hover.y < h - padB) {
    ctx.strokeStyle = withAlpha(muted, 0.4);
    ctx.setLineDash([3, 3]);
    ctx.beginPath();
    ctx.moveTo(hover.x, padT);
    ctx.lineTo(hover.x, h - padB);
    ctx.moveTo(8, hover.y);
    ctx.lineTo(8 + plotW, hover.y);
    ctx.stroke();
    ctx.setLineDash([]);
    const hv = min + ((h - padB - hover.y) / plotH) * (max - min);
    ctx.fillStyle = muted;
    ctx.font = "9.5px ui-monospace, monospace";
    ctx.fillText(hv.toLocaleString("en-US", { maximumFractionDigits: 0 }), 8 + plotW + 6, hover.y + 3);
  }
}

function withAlpha(color: string, alpha: number): string {
  // CSS vars are oklch(...) strings — append the alpha channel directly.
  if (color.startsWith("oklch(") && color.endsWith(")")) {
    return `${color.slice(0, -1)} / ${alpha})`;
  }
  if (color.startsWith("#") && (color.length === 7 || color.length === 4)) {
    const r = parseInt(color.slice(1, 3), 16);
    const g = parseInt(color.slice(3, 5), 16);
    const b = parseInt(color.slice(5, 7), 16);
    return `rgba(${r}, ${g}, ${b}, ${alpha})`;
  }
  return color;
}

function roundRect(ctx: CanvasRenderingContext2D, x: number, y: number, w: number, h: number, r: number): void {
  ctx.beginPath();
  ctx.moveTo(x + r, y);
  ctx.arcTo(x + w, y, x + w, y + h, r);
  ctx.arcTo(x + w, y + h, x, y + h, r);
  ctx.arcTo(x, y + h, x, y, r);
  ctx.arcTo(x, y, x + w, y, r);
  ctx.closePath();
}
