"use client";

/**
 * Account strip — equity, margin ladders, health, exposure at a glance.
 * The health bar visualizes equity vs initial/maintenance thresholds.
 */

import { memo } from "react";
import { useVenueStore, liveAccount } from "@/lib/venue-store";
import { usd, healthClass, healthLabel } from "@/lib/fmt";
import { cn } from "@/lib/utils";

export const AccountStrip = memo(function AccountStrip() {
  const snapshot = useVenueStore((s) => s.snapshot);
  const deltas = useVenueStore((s) => s.accountDeltas);
  const account = useVenueStore((s) => s.activeAccount);
  const a = liveAccount(snapshot, deltas, account);
  const descriptor = snapshot?.accountDescriptors.find((d) => d.id === account);

  if (!a) return null;

  const equity = Number(a.equity_quote_minor);
  const initial = Number(a.initial_quote_minor);
  const maintenance = Number(a.maintenance_quote_minor);
  const orderMargin = Number(a.order_margin_quote_minor);
  const denom = Math.max(1, initial, maintenance);
  const healthPct = Math.max(0, Math.min(100, (equity / denom) * 100));
  const mmPct = Math.max(0, Math.min(100, (maintenance / denom) * 100));
  const imPct = Math.max(0, Math.min(100, (initial / denom) * 100));

  return (
    <div className="h-14 shrink-0 border-b border-hairline bg-card/40 backdrop-blur-sm flex items-center gap-4 px-3 overflow-x-auto scroll-thin">
      <div className="flex items-center gap-2 shrink-0">
        <span className="w-2 h-2 rounded-full" style={{ background: descriptor?.color ?? "#888" }} />
        <span className="text-[12px] font-semibold">{descriptor?.name ?? `Sub ${account}`}</span>
        <span className={cn("text-[10px] font-medium", healthClass(a.health))}>
          ● {healthLabel(a.health)}
        </span>
      </div>

      {/* Health bar */}
      <div className="shrink-0 min-w-0 hidden md:flex items-center gap-2">
        <div className="relative h-2.5 w-40 rounded-full bg-muted overflow-hidden">
          <div
            className={cn(
              "absolute inset-y-0 left-0 rounded-full transition-all duration-500",
              a.health === "liquidation" ? "bg-down" : a.health === "restricted" ? "bg-amber-400" : "bg-up"
            )}
            style={{ width: `${healthPct}%` }}
          />
          <div className="absolute inset-y-0 w-px bg-amber-400/80" style={{ left: `${mmPct}%` }} title="Maintenance" />
          <div className="absolute inset-y-0 w-px bg-muted-foreground/60" style={{ left: `${imPct}%` }} title="Initial" />
        </div>
        <span className="num text-[10px] text-muted-foreground">{healthPct.toFixed(0)}% of IM</span>
      </div>

      <Metric label="Equity" value={usd(a.equity_quote_minor)} strong />
      <Metric label="Cash" value={usd(a.cash_quote_minor)} />
      <Metric label="Order margin" value={usd(BigInt.asUintN(64, a.order_margin_quote_minor))} />
      <Metric label="Initial" value={usd(a.initial_quote_minor)} muted />
      <Metric label="Maintenance" value={usd(a.maintenance_quote_minor)} muted />
      <Metric label="Fees paid" value={usd(a.fees_paid_quote_minor)} muted />
      <Metric
        label="Funding recv"
        value={usd(a.funding_received_quote_minor, { sign: true })}
        className={a.funding_received_quote_minor > 0n ? "text-up" : a.funding_received_quote_minor < 0n ? "text-down" : undefined}
      />
      <div className="flex-1 min-w-0" />
      <span className="hidden 2xl:block text-[9px] text-muted-foreground/50 font-mono shrink-0">
        SFPM scenario grid · MM = IM × 0.75
      </span>
      <span className="sr-only">{orderMargin} order margin</span>
    </div>
  );
});

function Metric({
  label,
  value,
  strong,
  muted,
  className,
}: {
  label: string;
  value: string;
  strong?: boolean;
  muted?: boolean;
  className?: string;
}) {
  return (
    <div className="flex flex-col leading-none shrink-0">
      <span className="text-[8.5px] uppercase tracking-wider text-muted-foreground/60 mb-0.5">{label}</span>
      <span
        className={cn(
          "num",
          strong ? "text-[13px] font-semibold" : "text-[11.5px]",
          muted && "text-muted-foreground",
          className
        )}
      >
        {value}
      </span>
    </div>
  );
}
