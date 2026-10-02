"use client";

/**
 * Market header — the active instrument's identity strip:
 * selector, last/mark, BBO spread, funding, IV, session stats.
 */

import { memo, useMemo, useState } from "react";
import { useVenueStore, marketRowFor } from "@/lib/venue-store";
import { usd, changePct, pct, bps, iv as fmtIv, shortSymbol } from "@/lib/fmt";
import type { MarketRow } from "@perp/types";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectLabel,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Search } from "lucide-react";
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";

export const MarketHeader = memo(function MarketHeader() {
  const snapshot = useVenueStore((s) => s.snapshot);
  const markets = useVenueStore((s) => s.markets);
  const activeSymbol = useVenueStore((s) => s.activeSymbol);
  const setActiveSymbol = useVenueStore((s) => s.setActiveSymbol);
  const [open, setOpen] = useState(false);

  const row = marketRowFor(markets, snapshot, activeSymbol);
  const change = row ? changePct(row) : null;

  const grouped = useMemo(() => {
    const perps = markets.filter((m) => m.kind === "perp");
    const options = markets
      .filter((m) => m.kind === "option")
      .sort((a, b) => (a.expiry_ts ?? 9e15) - (b.expiry_ts ?? 9e15) || Number((a.strike_quote_minor ?? 0n) - (b.strike_quote_minor ?? 0n)));
    return { perps, options };
  }, [markets]);

  return (
    <div className="h-12 shrink-0 border-b border-hairline bg-card/40 backdrop-blur-sm flex items-center gap-3 px-3">
      <Popover open={open} onOpenChange={setOpen}>
        <PopoverTrigger asChild>
          <Button
            variant="outline"
            size="sm"
            className="h-8 gap-2 font-medium border-hairline bg-transparent hover:bg-muted/70"
          >
            <Search className="w-3.5 h-3.5 opacity-60" />
            <span className="font-mono text-[12.5px]">{activeSymbol}</span>
          </Button>
        </PopoverTrigger>
        <PopoverContent className="w-[340px] p-0" align="start">
          <Command>
            <CommandInput placeholder="Search instruments — BTC-PERP, 80,000 calls, EVER…" />
            <CommandList className="max-h-[380px]">
              <CommandEmpty>No instruments match.</CommandEmpty>
              <CommandGroup heading="Perpetuals">
                {grouped.perps.map((m) => (
                  <MarketPickerItem key={m.symbol} row={m} active={m.symbol === activeSymbol} onPick={() => { setActiveSymbol(m.symbol); setOpen(false); }} />
                ))}
              </CommandGroup>
              <CommandGroup heading="Options — dated & everlasting">
                {grouped.options.map((m) => (
                  <MarketPickerItem key={m.symbol} row={m} active={m.symbol === activeSymbol} onPick={() => { setActiveSymbol(m.symbol); setOpen(false); }} />
                ))}
              </CommandGroup>
            </CommandList>
          </Command>
        </PopoverContent>
      </Popover>

      {row && (
        <>
          <HeaderStat label="Last" value={usd(row.mark_quote_minor)} big />
          <HeaderStat
            label="24h"
            value={change != null ? pct(change) : "—"}
            className={change != null ? (change >= 0 ? "text-up" : "text-down") : undefined}
          />
          <div className="hidden md:flex items-center gap-3">
            <HeaderStat
              label="Bid"
              value={row.best_bid_ticks != null ? usd(ticksToMinor(row, row.best_bid_ticks)) : "—"}
              className="text-up"
            />
            <HeaderStat
              label="Ask"
              value={row.best_ask_ticks != null ? usd(ticksToMinor(row, row.best_ask_ticks)) : "—"}
              className="text-down"
            />
            <HeaderStat
              label="Spread"
              value={row.spread_ticks != null ? usd(ticksToMinor(row, row.spread_ticks)) : "—"}
            />
          </div>
          <div className="hidden xl:flex items-center gap-3">
            {row.kind === "perp" ? (
              <HeaderStat
                label="Funding 8h"
                value={row.funding_rate_bps != null ? bps(row.funding_rate_bps) : "—"}
                className={
                  row.funding_rate_bps != null
                    ? row.funding_rate_bps > 0
                      ? "text-down"
                      : row.funding_rate_bps < 0
                        ? "text-up"
                        : undefined
                    : undefined
                }
              />
            ) : (
              <HeaderStat label="IV" value={fmtIv(row.iv)} />
            )}
            <HeaderStat label="Volume 24h" value={usd(row.volume_quote_minor, { compact: true })} />
          </div>
          <div className="hidden lg:flex items-center gap-1.5">
            {row.kind === "option" && (
              <>
                <Badge variant="outline" className="text-[9.5px] px-1.5 py-0 h-[18px] font-mono border-hairline">
                  {row.exercise_style === "american" ? "AMERICAN" : "EUROPEAN"}
                </Badge>
                <Badge variant="outline" className="text-[9.5px] px-1.5 py-0 h-[18px] font-mono border-hairline">
                  {row.variant === "everlasting" ? "EVERLASTING" : "DATED"}
                </Badge>
              </>
            )}
            {row.halted && (
              <Badge variant="destructive" className="text-[9.5px] px-1.5 py-0 h-[18px] font-mono">
                HALTED
              </Badge>
            )}
          </div>
        </>
      )}
      <div className="flex-1" />
      <p className="hidden 2xl:block text-[10px] text-muted-foreground/70 font-mono">
        {row?.kind === "option" ? "cash-settled · premium-unpaid · marks never listen to the book" : "linear perp · premium + interest funding"}
      </p>
    </div>
  );
});

function ticksToMinor(row: MarketRow, ticks: number): bigint {
  const inst = useVenueStore.getState().snapshot?.instruments.find((i) => i.symbol === row.symbol);
  if (!inst) return 0n;
  return inst.tick_size_quote_minor * BigInt(ticks);
}

function HeaderStat({
  label,
  value,
  big,
  className,
}: {
  label: string;
  value: string;
  big?: boolean;
  className?: string;
}) {
  return (
    <div className="flex flex-col leading-none min-w-0">
      <span className="text-[9px] uppercase tracking-wider text-muted-foreground/70 mb-0.5">{label}</span>
      <span className={`num ${big ? "text-[15px] font-semibold" : "text-[12px]"} ${className ?? ""}`}>{value}</span>
    </div>
  );
}

function MarketPickerItem({ row, active, onPick }: { row: MarketRow; active: boolean; onPick: () => void }) {
  return (
    <CommandItem
      value={`${row.symbol} ${row.label}`}
      onSelect={onPick}
      className={`gap-2 ${active ? "bg-primary/10" : ""}`}
    >
      <span className="font-mono text-[11px] min-w-0 truncate">{shortSymbol(row.symbol)}</span>
      <span className="num text-[11px] ml-auto text-muted-foreground">{usd(row.mark_quote_minor)}</span>
      {row.kind === "option" && (
        <span className="text-[8.5px] font-mono text-muted-foreground/60 uppercase">
          {row.exercise_style === "american" ? "AM" : "EU"}
          {row.variant === "everlasting" ? "·EV" : ""}
        </span>
      )}
    </CommandItem>
  );
}
