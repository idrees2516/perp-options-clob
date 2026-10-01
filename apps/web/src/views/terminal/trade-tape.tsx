"use client";

/**
 * The trade tape — every print on the active instrument, newest first.
 * Aggressor side colors the row (maker_side semantics from the protocol).
 */

import { memo } from "react";
import { useVenueStore } from "@/lib/venue-store";
import { usd, sizeBase } from "@/lib/fmt";

export const TradeTape = memo(function TradeTape() {
  const activeSymbol = useVenueStore((s) => s.activeSymbol);
  const prints = useVenueStore((s) => s.prints);
  const snapshot = useVenueStore((s) => s.snapshot);

  const inst = snapshot?.instruments.find((i) => i.symbol === activeSymbol) ?? null;
  const rows = prints.filter((p) => p.symbol === activeSymbol).slice(-90).reverse();

  return (
    <div className="h-full flex flex-col min-h-0">
      <div className="h-7 shrink-0 flex items-center justify-between px-3 border-b border-hairline">
        <span className="text-[9px] uppercase tracking-wider text-muted-foreground/70">Tape — prints</span>
        <span className="text-[9px] text-muted-foreground/50 font-mono">{rows.length} shown</span>
      </div>
      <div className="flex-1 min-h-0 overflow-y-auto scroll-thin" role="log" aria-label="Recent trades">
        {rows.length === 0 ? (
          <div className="h-full flex items-center justify-center text-[10px] text-muted-foreground/50">
            awaiting prints…
          </div>
        ) : (
          rows.map((p) => {
            const priceMinor = inst ? inst.tick_size_quote_minor * BigInt(p.price_ticks) : 0n;
            const takerSide = p.maker_side === "bid" ? "ask" : "bid";
            return (
              <div
                key={`${p.seq}`}
                className="grid grid-cols-[1fr_auto_auto] gap-3 px-3 h-[21px] items-center text-[11px]"
              >
                <span className={`num ${takerSide === "bid" ? "text-up" : "text-down"}`}>
                  {usd(priceMinor)}
                </span>
                <span className="num text-muted-foreground text-right">
                  {inst ? sizeBase(p.qty_lots, inst) : p.qty_lots}
                </span>
                <span className="num text-[9.5px] text-muted-foreground/70">
                  {new Date(p.ts).toISOString().slice(11, 19)}
                </span>
              </div>
            );
          })
        )}
      </div>
    </div>
  );
});
