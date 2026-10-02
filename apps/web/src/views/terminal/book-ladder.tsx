"use client";

/**
 * The book ladder — L2 depth rendered as a price ladder with heat bars.
 * Rows are memoized per price level; only changed rows re-render.
 * Click a level to load its price into the ticket.
 */

import { memo, useMemo } from "react";
import { useVenueStore } from "@/lib/venue-store";
import { usd, sizeBase } from "@/lib/fmt";
import type { Instrument, Level } from "@perp/types";
import { setTicketPrice } from "./order-entry";

const ROWS = 11;

export const BookLadder = memo(function BookLadder() {
  const activeSymbol = useVenueStore((s) => s.activeSymbol);
  const book = useVenueStore((s) => s.books[s.activeSymbol]);
  const snapshot = useVenueStore((s) => s.snapshot);
  const inst = useMemo<Instrument | null>(
    () => snapshot?.instruments.find((i) => i.symbol === activeSymbol) ?? null,
    [snapshot, activeSymbol],
  );

  const bids = book?.bids.slice(0, ROWS) ?? [];
  const asks = book?.asks.slice(0, ROWS) ?? [];

  const maxSize = useMemo(() => {
    let m = 1;
    for (const l of [...bids, ...asks]) m = Math.max(m, l.lots);
    return m;
  }, [bids, asks]);

  const cum = useMemo(() => {
    let c = 0;
    const bidCum = bids.map((l) => (c += l.lots));
    c = 0;
    const askCum = asks.map((l) => (c += l.lots));
    return { bidCum, askCum };
  }, [bids, asks]);

  const spreadTicks =
    bids.length && asks.length ? asks[0]!.price_ticks - bids[0]!.price_ticks : null;
  const spreadMinor = inst && spreadTicks != null ? inst.tick_size_quote_minor * BigInt(spreadTicks) : null;
  const midMinor =
    inst && bids.length && asks.length
      ? (inst.tick_size_quote_minor * (BigInt(bids[0]!.price_ticks) + BigInt(asks[0]!.price_ticks))) / 2n
      : null;

  return (
    <div className="h-full flex flex-col min-h-0 bg-transparent" aria-label={`Order book ${activeSymbol}`}>
      {/* Header */}
      <div className="h-7 shrink-0 grid grid-cols-[1.15fr_0.85fr_0.85fr] items-center px-2.5 border-b border-hairline text-[9px] uppercase tracking-wider text-muted-foreground/70">
        <span>Price (USD)</span>
        <span className="text-right">Size</span>
        <span className="text-right">Cum</span>
      </div>

      {/* Asks (desc → lowest at the bottom near mid) */}
      <div className="flex-1 min-h-0 flex flex-col justify-end overflow-hidden">
        {[...asks].reverse().map((l, i) => (
          <BookRow
            key={`a-${l.price_ticks}`}
            level={l}
            side="ask"
            inst={inst}
            maxSize={maxSize}
            cum={cum.askCum[asks.length - 1 - i] ?? 0}
          />
        ))}
      </div>

      {/* Mid */}
      <div className="h-9 shrink-0 border-y border-hairline bg-muted/30 flex items-center justify-between px-2.5">
        <span className="num text-[13px] font-semibold">{usd(midMinor)}</span>
        <span className="num text-[10px] text-muted-foreground" >
          {spreadMinor != null ? `spread ${usd(spreadMinor)}` : "—"}
        </span>
      </div>

      {/* Bids */}
      <div className="flex-1 min-h-0 flex flex-col justify-start overflow-hidden">
        {bids.map((l, i) => (
          <BookRow
            key={`b-${l.price_ticks}`}
            level={l}
            side="bid"
            inst={inst}
            maxSize={maxSize}
            cum={cum.bidCum[i] ?? 0}
          />
        ))}
      </div>
    </div>
  );
});

interface BookRowProps {
  level: Level;
  side: "bid" | "ask";
  inst: Instrument | null;
  maxSize: number;
  cum: number;
}

const BookRow = memo(function BookRow({ level, side, inst, maxSize, cum }: BookRowProps) {
  const pct = Math.min(100, (level.lots / maxSize) * 100);
  const priceMinor = inst ? inst.tick_size_quote_minor * BigInt(level.price_ticks) : 0n;
  return (
    <button
      onClick={() => setTicketPrice(level.price_ticks)}
      className="relative w-full h-[26px] grid grid-cols-[1.15fr_0.85fr_0.85fr] items-center px-2.5 text-[11.5px] hover:bg-muted/40 transition-colors group"
      aria-label={`${side === "bid" ? "Bid" : "Ask"} ${level.price_ticks} — click to load price`}
    >
      <span
        className={`depth-bar ${side === "bid" ? "bg-up/10 group-hover:bg-up/20" : "bg-down/10 group-hover:bg-down/20"}`}
        style={{ width: `${pct}%` }}
      />
      <span className={`num relative z-10 text-left ${side === "bid" ? "text-up" : "text-down"}`}>
        {usd(priceMinor)}
      </span>
      <span className="num relative z-10 text-right text-muted-foreground group-hover:text-foreground transition-colors">
        {inst ? sizeBase(level.lots, inst) : level.lots}
      </span>
      <span className="num relative z-10 text-right text-muted-foreground/60">{cum.toLocaleString()}</span>
    </button>
  );
});
