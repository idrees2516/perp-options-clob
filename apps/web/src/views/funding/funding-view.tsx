"use client";

/**
 * Funding — the perp's economic tether.
 * Current rate + next 8h boundary, an inline-SVG history chart
 * (premium + interest, ±75bp cap), the full settlement ledger,
 * everlasting roll rates, and the zero-sum contract in plain words.
 */

import { memo, useMemo } from "react";
import { useVenueStore } from "@/lib/venue-store";
import { usd, bps, shortSymbol, simTime, simDate, relTime } from "@/lib/fmt";
import type { FundingRecord } from "@perp/types";
import { Activity, Coins, Percent, Scale, Sigma, Timer } from "lucide-react";
import { cn } from "@/lib/utils";

const PERP = "BTC-PERP";
const FUNDING_INTERVAL_MS = 8 * 60 * 60 * 1000;

export const FundingView = memo(function FundingView() {
  const snapshot = useVenueStore((s) => s.snapshot);

  const now = snapshot?.meta.now ?? Date.now();
  const intervals = snapshot?.stats.funding_intervals ?? 0;

  const perpHistory = useMemo(
    () => (snapshot?.fundingHistory ?? []).filter((r) => r.symbol === PERP),
    [snapshot],
  );
  const last = perpHistory.length > 0 ? perpHistory[perpHistory.length - 1]! : null;

  /** Next 8h boundary on the sim clock: ceil(now / 8h) × 8h. */
  const nextBoundary = Math.ceil(now / FUNDING_INTERVAL_MS) * FUNDING_INTERVAL_MS;

  const everlastings = useMemo(
    () => (snapshot?.markets ?? []).filter((m) => m.variant === "everlasting"),
    [snapshot],
  );
  const lastRollBySymbol = useMemo(() => {
    const m = new Map<string, FundingRecord>();
    for (const r of snapshot?.fundingHistory ?? []) m.set(r.symbol, r);
    return m;
  }, [snapshot]);

  const rateClass =
    last == null
      ? "text-muted-foreground"
      : last.rate_bps > 0
        ? "text-down"
        : last.rate_bps < 0
          ? "text-up"
          : "text-muted-foreground";

  return (
    <div className="h-full flex flex-col min-h-0">
      {/* Toolbar */}
      <div className="h-11 shrink-0 flex items-center gap-2.5 px-3 border-b border-hairline">
        <div className="flex items-center gap-1.5 px-2 py-0.5 rounded-md bg-muted/60 border border-hairline">
          <span className="w-1 h-1 rounded-full bg-chart-3" aria-hidden />
          <span className="text-[10px] text-muted-foreground tracking-wide uppercase font-mono">
            {PERP}
          </span>
        </div>
        <span className="text-[10px] text-muted-foreground">8h funding interval · premium + interest</span>
        <div className="flex-1" />
        <p className="text-[10px] text-muted-foreground/60 font-mono">
          {intervals} intervals settled · {perpHistory.length} records in window
        </p>
      </div>

      <div className="flex-1 min-h-0 overflow-auto scroll-thin p-3">
        {/* Stat tiles */}
        <section aria-label="Funding rates" className="grid grid-cols-2 lg:grid-cols-4 gap-2 mb-3">
          <StatTile
            icon={<Percent className="w-3.5 h-3.5" aria-hidden />}
            label="Current rate"
            value={last ? bps(last.rate_bps) : "—"}
            valueClass={rateClass}
            sub={last ? `settled ${relTime(last.ts, now)}` : "first interval pending"}
          />
          <StatTile
            icon={<Timer className="w-3.5 h-3.5" aria-hidden />}
            label="Next interval"
            value={relTime(nextBoundary, now)}
            sub={`boundary ${simTime(nextBoundary)} UTC`}
          />
          <StatTile
            icon={<Activity className="w-3.5 h-3.5" aria-hidden />}
            label="Premium"
            value={last ? bps(last.premium_bps) : "—"}
            sub="markTWAP − indexTWAP · clamp ±5bp"
          />
          <StatTile
            icon={<Coins className="w-3.5 h-3.5" aria-hidden />}
            label="Interest"
            value={last ? bps(last.interest_bps) : "—"}
            sub="per 8h interval"
          />
        </section>

        {/* Chart + everlasting rolls */}
        <div className="grid grid-cols-1 xl:grid-cols-12 gap-3 mb-3">
          <section aria-label="Funding rate history chart" className="panel xl:col-span-8 overflow-hidden flex flex-col">
            <header className="px-3 py-2 border-b border-hairline flex flex-wrap items-center gap-x-3 gap-y-1">
              <h2 className="text-[11px] font-semibold tracking-tight">
                Funding rate — 8h intervals (premium + interest, ±75bp cap)
              </h2>
              <div className="flex-1" />
              <span className="flex items-center gap-1 text-[9.5px] text-muted-foreground">
                <span className="w-2 h-2 rounded-sm bg-down/80" aria-hidden />
                rate &gt; 0 · longs pay
              </span>
              <span className="flex items-center gap-1 text-[9.5px] text-muted-foreground">
                <span className="w-2 h-2 rounded-sm bg-up/80" aria-hidden />
                rate &lt; 0 · shorts pay
              </span>
            </header>
            <div className="p-3">
              <FundingBars records={perpHistory} />
              {perpHistory.length > 1 && (
                <div className="flex justify-between text-[9px] text-muted-foreground/60 font-mono mt-1.5 px-0.5">
                  <span>
                    {simDate(perpHistory[0]!.ts)} · {simTime(perpHistory[0]!.ts)}
                  </span>
                  <span>
                    {simDate(perpHistory[perpHistory.length - 1]!.ts)} ·{" "}
                    {simTime(perpHistory[perpHistory.length - 1]!.ts)}
                  </span>
                </div>
              )}
            </div>
          </section>

          <section aria-label="Everlasting rolls" className="panel xl:col-span-4 overflow-hidden flex flex-col">
            <header className="px-3 py-2 border-b border-hairline flex items-center gap-2">
              <h2 className="text-[11px] font-semibold tracking-tight">Everlasting rolls</h2>
              <div className="flex-1" />
              <span className="text-[9px] font-mono text-muted-foreground/60">{everlastings.length} strikes</span>
            </header>
            <div className="flex-1 min-h-0 overflow-auto scroll-thin max-h-72">
              {everlastings.length === 0 ? (
                <div className="h-full min-h-28 flex items-center justify-center text-muted-foreground/50 text-[11px] px-4 text-center">
                  No everlasting strikes listed — auto-listing keeps the perpetual chain around spot.
                </div>
              ) : (
                everlastings.map((m) => {
                  const roll = lastRollBySymbol.get(m.symbol) ?? null;
                  return (
                    <div
                      key={m.symbol}
                      className="flex items-center gap-2 px-3 py-1.5 border-b border-hairline/40 hover:bg-muted/25"
                    >
                      <span className="font-mono text-[11px] truncate">{shortSymbol(m.symbol)}</span>
                      <div className="flex-1 min-w-0" />
                      <span
                        className={cn(
                          "num text-[11px]",
                          (m.funding_rate_bps ?? 0) > 0
                            ? "text-down"
                            : (m.funding_rate_bps ?? 0) < 0
                              ? "text-up"
                              : "text-muted-foreground",
                        )}
                      >
                        {m.funding_rate_bps != null ? bps(m.funding_rate_bps) : "—"}
                      </span>
                      <span className="num text-[9.5px] text-muted-foreground/60 w-[64px] text-right">
                        {roll ? relTime(roll.ts, now) : "—"}
                      </span>
                    </div>
                  );
                })
              )}
            </div>
            <footer className="px-3 py-2 border-t border-hairline bg-panel-2/40 text-[9.5px] text-muted-foreground">
              1h roll · effective maturity ≈ 24h · longs pay the premium TWAP
            </footer>
          </section>
        </div>

        {/* History table */}
        <section aria-label="Funding history" className="panel mb-3 overflow-hidden">
          <header className="px-3 py-2 border-b border-hairline flex items-center gap-2">
            <h2 className="text-[11px] font-semibold tracking-tight">History</h2>
            <span className="text-[9.5px] text-muted-foreground/70 hidden sm:inline">
              every settlement with its TWAP components
            </span>
            <div className="flex-1" />
            <span className="text-[9px] font-mono text-muted-foreground/60">newest first</span>
          </header>
          <div className="max-h-96 overflow-auto scroll-thin">
            <table className="w-full text-[11.5px] border-collapse">
              <thead>
                <tr>
                  <Th>Time</Th>
                  <Th>Symbol</Th>
                  <Th right>Rate</Th>
                  <Th right>Premium</Th>
                  <Th right>Interest</Th>
                  <Th right>Mark TWAP</Th>
                  <Th right>Index TWAP</Th>
                </tr>
              </thead>
              <tbody>
                {perpHistory.length === 0 && (
                  <EmptyRow cols={7} text="No funding records yet — the first 8h interval is still accruing." />
                )}
                {[...perpHistory].reverse().map((r) => (
                  <tr key={`${r.symbol}-${r.ts}`} className="hover:bg-muted/30 border-b border-hairline/40">
                    <Td className="text-muted-foreground">
                      {simTime(r.ts)}{" "}
                      <span className="text-muted-foreground/50 text-[10px]">· {simDate(r.ts)}</span>
                    </Td>
                    <Td className="font-mono text-[11px]">{shortSymbol(r.symbol)}</Td>
                    <Td
                      right
                      className={
                        r.rate_bps > 0 ? "text-down" : r.rate_bps < 0 ? "text-up" : "text-muted-foreground"
                      }
                    >
                      {bps(r.rate_bps)}
                    </Td>
                    <Td right className="text-muted-foreground">
                      {bps(r.premium_bps)}
                    </Td>
                    <Td right className="text-muted-foreground">
                      {bps(r.interest_bps)}
                    </Td>
                    <Td right>{usd(r.twap_mark_quote_minor)}</Td>
                    <Td right className="text-muted-foreground">
                      {usd(r.twap_index_quote_minor)}
                    </Td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </section>

        {/* Explainer */}
        <section aria-label="How funding works" className="panel p-3">
          <h2 className="text-[11px] font-semibold tracking-tight mb-2.5 flex items-center gap-1.5">
            <Sigma className="w-3.5 h-3.5 text-primary" aria-hidden />
            Funding tethers the synthetic to the real
          </h2>
          <ul className="grid grid-cols-1 md:grid-cols-3 gap-2.5 list-none p-0 m-0">
            <li className="panel-2 p-2.5">
              <p className="text-[9px] uppercase tracking-wider text-muted-foreground mb-1.5">Formula</p>
              <p className="text-[11px] font-mono leading-relaxed text-foreground/90">
                rate = clamp(interest + clamp((markTWAP − indexTWAP) / index, ±5bp), ±75bp)
              </p>
            </li>
            <li className="panel-2 p-2.5">
              <p className="text-[9px] uppercase tracking-wider text-muted-foreground mb-1.5 flex items-center gap-1">
                <Scale className="w-2.5 h-2.5" aria-hidden />
                Direction
              </p>
              <p className="text-[11px] leading-relaxed">
                Longs pay shorts when the rate is positive — the sign only flips who pays.
              </p>
            </li>
            <li className="panel-2 p-2.5">
              <p className="text-[9px] uppercase tracking-wider text-muted-foreground mb-1.5 flex items-center gap-1">
                <Activity className="w-2.5 h-2.5" aria-hidden />
                Zero-sum
              </p>
              <p className="text-[11px] leading-relaxed">
                Exactly zero-sum — Σ credits = 0, with the rounding residual assigned to the largest payer.
              </p>
            </li>
          </ul>
        </section>
      </div>
    </div>
  );
});

/* ─────────────────────────── chart ─────────────────────────── */

/** Inline-SVG bar chart — no chart dependency, terminal-native. */
function FundingBars({ records }: { records: FundingRecord[] }) {
  const bars = records.slice(-60);
  if (bars.length === 0) {
    return (
      <div className="h-44 flex items-center justify-center text-muted-foreground/50 text-[11px]">
        No intervals settled yet — the chart arms at the first 8h boundary.
      </div>
    );
  }

  const W = 720;
  const H = 176;
  const PAD_L = 38;
  const PAD_R = 8;
  const PAD_T = 8;
  const PAD_B = 8;
  const Y_MAX = 75;
  const plotW = W - PAD_L - PAD_R;
  const plotH = H - PAD_T - PAD_B;
  const y = (v: number) => PAD_T + plotH * (1 - (v + Y_MAX) / (2 * Y_MAX));
  const bw = plotW / bars.length;
  const ticks = [75, 37.5, 0, -37.5, -75];

  return (
    <svg
      viewBox={`0 0 ${W} ${H}`}
      className="w-full"
      role="img"
      aria-label={`BTC-PERP funding rate history, ${bars.length} intervals, ±75bp scale`}
    >
      {/* gridlines + bps axis labels */}
      {ticks.map((t) => (
        <g key={t}>
          <line
            x1={PAD_L}
            x2={W - PAD_R}
            y1={y(t)}
            y2={y(t)}
            className={t === 0 ? "stroke-hairline" : "stroke-hairline/60"}
            strokeWidth={1}
            strokeDasharray={t === 0 ? undefined : "3 5"}
          />
          <text
            x={PAD_L - 6}
            y={y(t) + 3}
            textAnchor="end"
            className="fill-muted-foreground text-[9px] num"
          >
            {t > 0 ? `+${t}` : `${t}`}
          </text>
        </g>
      ))}
      {/* bars */}
      {bars.map((r, i) => {
        const x = PAD_L + i * bw + Math.min(1.5, bw * 0.15);
        const w = Math.max(1.5, bw - Math.min(3, bw * 0.3));
        const y0 = y(0);
        const yv = y(Math.max(-Y_MAX, Math.min(Y_MAX, r.rate_bps)));
        const top = Math.min(y0, yv);
        const h = Math.max(1.2, Math.abs(yv - y0));
        return (
          <rect
            key={r.ts}
            x={x}
            y={top}
            width={w}
            height={h}
            rx={0.5}
            className={r.rate_bps >= 0 ? "fill-down" : "fill-up"}
            fillOpacity={0.8}
          >
            <title>
              {`${simDate(r.ts)} ${simTime(r.ts)} · ${bps(r.rate_bps)} (premium ${bps(r.premium_bps)}, interest ${bps(r.interest_bps)})`}
            </title>
          </rect>
        );
      })}
    </svg>
  );
}

/* ─────────────────────────── local bits ─────────────────────────── */

function StatTile({
  icon,
  label,
  value,
  valueClass,
  sub,
}: {
  icon: React.ReactNode;
  label: string;
  value: string;
  valueClass?: string;
  sub?: string;
}) {
  return (
    <div className="panel p-3">
      <p className="text-[9px] uppercase tracking-wider text-muted-foreground flex items-center gap-1">
        <span className="text-muted-foreground/60">{icon}</span>
        {label}
      </p>
      <p className={cn("num text-lg font-semibold leading-tight mt-0.5", valueClass)}>{value}</p>
      {sub && <p className="text-[9.5px] text-muted-foreground/70 mt-0.5">{sub}</p>}
    </div>
  );
}

function Th({ children, right }: { children: React.ReactNode; right?: boolean }) {
  return (
    <th
      className={`sticky top-0 z-10 bg-panel backdrop-blur-sm text-[9px] uppercase tracking-wider text-muted-foreground/70 font-medium py-1.5 px-2.5 border-b border-hairline whitespace-nowrap ${
        right ? "text-right" : "text-left"
      }`}
    >
      {children}
    </th>
  );
}

function Td({ children, right, className }: { children: React.ReactNode; right?: boolean; className?: string }) {
  return (
    <td className={`py-1.5 px-2.5 whitespace-nowrap ${right ? "text-right num" : ""} ${className ?? ""}`}>{children}</td>
  );
}

function EmptyRow({ cols, text }: { cols: number; text: string }) {
  return (
    <tr>
      <td colSpan={cols} className="py-10 text-center text-muted-foreground/50 text-[11px]">
        {text}
      </td>
    </tr>
  );
}
