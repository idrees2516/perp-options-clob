"use client";

/**
 * Status bar — the venue's heartbeat at the bottom edge:
 * engine stats, revenue router split, journal cursor, market-data health.
 */

import { memo } from "react";
import { useVenueStore } from "@/lib/venue-store";
import { usdCompact, simDate } from "@/lib/fmt";

export const StatusBar = memo(function StatusBar() {
  const snapshot = useVenueStore((s) => s.snapshot);
  const journalLen = useVenueStore((s) => s.journal.length);
  const books = useVenueStore((s) => s.books);
  const activeSymbol = useVenueStore((s) => s.activeSymbol);

  const stats = snapshot?.stats;
  const meta = snapshot?.meta;
  const book = books[activeSymbol];
  const desynced = book?.desynced ?? false;
  const rev = stats?.revenue;
  const revTotal =
    rev != null
      ? rev.house_quote_minor + rev.insurance_quote_minor + rev.buyback_quote_minor
      : 0n;

  return (
    <footer className="h-7 shrink-0 border-t border-hairline bg-card/60 backdrop-blur-md flex items-center gap-3 px-3 text-[10.5px] text-muted-foreground z-20">
      <span className="flex items-center gap-1.5">
        <span className={`w-1.5 h-1.5 rounded-full ${meta?.connected ? "bg-up" : "bg-down"} ${meta?.connected ? "pulse-dot" : ""}`} />
        <span className="font-mono">{meta?.transport === "sim-worker" ? "SIM" : "GATEWAY"}</span>
      </span>
      <Divider />
      <Stat label="events" value={stats ? stats.events.toLocaleString() : "—"} />
      <Stat label="trades" value={stats ? stats.trades.toLocaleString() : "—"} />
      <Stat label="volume" value={stats ? usdCompact(stats.notional_traded_quote_minor) : "—"} />
      <Divider />
      <Stat
        label="revenue 60/30/10"
        value={
          revTotal > 0n && rev
            ? `${((Number(rev.house_quote_minor) / Number(revTotal)) * 100).toFixed(0)}/${(
                (Number(rev.insurance_quote_minor) / Number(revTotal)) * 100
              ).toFixed(0)}/${((Number(rev.buyback_quote_minor) / Number(revTotal)) * 100).toFixed(0)}`
            : "—"
        }
      />
      <Stat label="insurance" value={stats ? usdCompact(stats.insurance_balance) : "—"} />
      <Divider />
      <Stat label="journal" value={journalLen ? `+${journalLen.toLocaleString()} buffered` : "—"} />
      <Stat
        label="mktdata"
        value={
          book
            ? `seq ${book.seq.toLocaleString()}${desynced ? " · RESYNCING" : ""}`
            : "—"
        }
        className={desynced ? "text-amber-400" : undefined}
      />
      <div className="flex-1" />
      <Stat label="engine" value={meta ? `${meta.ticks.toLocaleString()} ticks` : "—"} />
      <Stat label="epoch" value={meta ? simDate(meta.now) : "—"} />
      <span className="font-mono opacity-70 truncate">seed {meta?.seed ?? "—"}</span>
    </footer>
  );
});

function Stat({ label, value, className }: { label: string; value: string; className?: string }) {
  return (
    <span className="flex items-baseline gap-1.5 whitespace-nowrap">
      <span className="uppercase tracking-wider opacity-60">{label}</span>
      <span className={`num ${className ?? ""}`}>{value}</span>
    </span>
  );
}

function Divider() {
  return <span className="w-px h-3.5 bg-hairline" />;
}
