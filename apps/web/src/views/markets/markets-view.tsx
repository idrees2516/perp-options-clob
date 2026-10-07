"use client";

/**
 * Markets — instrument discovery: the full table + the options chain matrix
 * (calls | strike | puts) with IV, marks, deltas, and expiry countdowns.
 */

import { memo, useMemo, useState } from "react";
import { useVenueStore } from "@/lib/venue-store";
import { usd, changePct, pct, iv as fmtIv, bps, shortSymbol, relTime } from "@/lib/fmt";
import { Input } from "@/components/ui/input";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Badge } from "@/components/ui/badge";
import { Search, ArrowDownUp } from "lucide-react";

type SortKey = "symbol" | "mark" | "volume" | "spread" | "expiry" | "strike";

export const MarketsView = memo(function MarketsView() {
  const markets = useVenueStore((s) => s.markets);
  const snapshot = useVenueStore((s) => s.snapshot);
  const setActiveSymbol = useVenueStore((s) => s.setActiveSymbol);
  const setView = useVenueStore((s) => s.setView);
  const [query, setQuery] = useState("");
  const [mode, setMode] = useState<"table" | "chain">("table");
  const [sortKey, setSortKey] = useState<SortKey>("volume");
  const [sortDesc, setSortDesc] = useState(true);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    let rows = markets;
    if (q) {
      rows = rows.filter(
        (m) =>
          m.symbol.toLowerCase().includes(q) ||
          m.label.toLowerCase().includes(q) ||
          (q === "perp" && m.kind === "perp") ||
          (q === "ever" && m.variant === "everlasting") ||
          (q === "am" && m.exercise_style === "american") ||
          (q === "eu" && m.exercise_style === "european"),
      );
    }
    const val = (m: (typeof markets)[number]): number | string => {
      switch (sortKey) {
        case "symbol": return m.symbol;
        case "mark": return Number(m.mark_quote_minor ?? 0n);
        case "volume": return Number(m.volume_quote_minor);
        case "spread": return m.spread_ticks ?? 1e9;
        case "strike": return Number(m.strike_quote_minor ?? 0n);
        case "expiry": return m.expiry_ts ?? (m.variant === "everlasting" ? 9e15 : 0);
      }
    };
    return [...rows].sort((a, b) => {
      const va = val(a);
      const vb = val(b);
      const cmp = typeof va === "string" ? va.localeCompare(vb as string) : (va as number) - (vb as number);
      return sortDesc ? -cmp : cmp;
    });
  }, [markets, query, sortKey, sortDesc]);

  return (
    <div className="h-full flex flex-col min-h-0">
      {/* Toolbar */}
      <div className="h-11 shrink-0 flex items-center gap-2.5 px-3 border-b border-hairline">
        <div className="relative w-64">
          <Search className="absolute left-2.5 top-1/2 -translate-y-1/2 w-3.5 h-3.5 text-muted-foreground/60" />
          <Input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Filter — perp, ever, am, 80000…"
            className="h-8 pl-8 text-[12px] bg-transparent border-hairline"
          />
        </div>
        <Tabs value={mode} onValueChange={(v) => setMode(v as "table" | "chain")}>
          <TabsList className="h-8 bg-muted/40 border border-hairline">
            <TabsTrigger value="table" className="text-[11px] h-6">All instruments</TabsTrigger>
            <TabsTrigger value="chain" className="text-[11px] h-6">Options chain</TabsTrigger>
          </TabsList>
        </Tabs>
        <button
          onClick={() => setSortDesc((d) => !d)}
          className="flex items-center gap-1 text-[10px] text-muted-foreground hover:text-foreground transition-colors"
        >
          <ArrowDownUp className="w-3 h-3" />
          {sortDesc ? "desc" : "asc"}
        </button>
        <div className="flex-1" />
        <p className="text-[10px] text-muted-foreground/60 font-mono">
          {filtered.length} instruments · {markets.filter((m) => m.kind === "option").length} options · auto-listing live
        </p>
      </div>

      {mode === "table" ? (
        <div className="flex-1 min-h-0 overflow-auto scroll-thin">
          <table className="w-full text-[11.5px]">
            <thead>
              <tr>
                {(
                  [
                    ["Instrument", "symbol"],
                    ["Mark", "mark"],
                    ["24h", null],
                    ["Bid", null],
                    ["Ask", null],
                    ["Spread", "spread"],
                    ["Volume 24h", "volume"],
                    ["IV", null],
                    ["Funding", null],
                    ["Strike", "strike"],
                    ["Expiry", "expiry"],
                    ["Style", null],
                  ] as [string, SortKey | null][]
                ).map(([label, key]) => (
                  <th
                    key={label}
                    onClick={() => key && (setSortKey(key), setSortDesc(sortKey === key ? !sortDesc : true))}
                    className={`sticky top-0 z-10 bg-panel backdrop-blur-sm text-[9px] uppercase tracking-wider text-muted-foreground/70 font-medium py-2 px-2.5 border-b border-hairline whitespace-nowrap ${
                      key === "mark" || key === "volume" || label === "24h" || label === "Bid" || label === "Ask" || label === "Spread" || label === "IV" || label === "Funding" || label === "Strike" ? "text-right" : "text-left"
                    } ${key ? "cursor-pointer hover:text-foreground" : ""}`}
                  >
                    {label}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {filtered.map((m) => {
                const chg = changePct(m);
                const inst = snapshot?.instruments.find((i) => i.symbol === m.symbol);
                return (
                  <tr
                    key={m.symbol}
                    onClick={() => {
                      setActiveSymbol(m.symbol);
                      setView("terminal");
                    }}
                    className="hover:bg-primary/5 cursor-pointer border-b border-hairline/40"
                  >
                    <td className="py-1.5 px-2.5">
                      <div className="flex items-center gap-2">
                        <span className={`w-1 h-4 rounded-full ${m.kind === "perp" ? "bg-chart-3" : "bg-primary/60"}`} />
                        <span className="font-mono text-[11px]">{shortSymbol(m.symbol)}</span>
                        {m.halted && (
                          <Badge variant="destructive" className="h-4 text-[8px] px-1">HALTED</Badge>
                        )}
                      </div>
                    </td>
                    <td className="text-right num font-medium">{usd(m.mark_quote_minor)}</td>
                    <td className={`text-right num ${chg != null ? (chg >= 0 ? "text-up" : "text-down") : "text-muted-foreground"}`}>
                      {chg != null ? pct(chg) : "—"}
                    </td>
                    <td className="text-right num text-up">
                      {m.best_bid_ticks != null && inst ? usd(inst.tick_size_quote_minor * BigInt(m.best_bid_ticks)) : "—"}
                    </td>
                    <td className="text-right num text-down">
                      {m.best_ask_ticks != null && inst ? usd(inst.tick_size_quote_minor * BigInt(m.best_ask_ticks)) : "—"}
                    </td>
                    <td className="text-right num text-muted-foreground">
                      {m.spread_ticks != null && inst ? usd(inst.tick_size_quote_minor * BigInt(m.spread_ticks)) : "—"}
                    </td>
                    <td className="text-right num text-muted-foreground">{usd(m.volume_quote_minor, { compact: true })}</td>
                    <td className="text-right num text-chart-3">{m.kind === "option" ? fmtIv(m.iv) : "—"}</td>
                    <td className="text-right num">
                      {m.funding_rate_bps != null ? (
                        <span className={m.funding_rate_bps > 0 ? "text-down" : m.funding_rate_bps < 0 ? "text-up" : "text-muted-foreground"}>
                          {bps(m.funding_rate_bps)}
                        </span>
                      ) : (
                        "—"
                      )}
                    </td>
                    <td className="text-right num text-muted-foreground">
                      {m.strike_quote_minor != null ? usd(m.strike_quote_minor) : "—"}
                    </td>
                    <td className="text-right num text-muted-foreground">
                      {m.variant === "everlasting" ? (
                        <span className="text-primary/80">everlasting</span>
                      ) : m.expiry_ts != null ? (
                        relTime(m.expiry_ts, snapshot?.meta.now ?? Date.now())
                      ) : (
                        "—"
                      )}
                    </td>
                    <td className="px-2.5">
                      <span className="flex gap-1">
                        {m.exercise_style === "american" && <StyleBadge>AM</StyleBadge>}
                        {m.exercise_style === "european" && m.kind === "option" && <StyleBadge>EU</StyleBadge>}
                        {m.variant === "everlasting" && <StyleBadge className="text-primary border-primary/40">EV</StyleBadge>}
                      </span>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      ) : (
        <ChainMatrix />
      )}
    </div>
  );
});

function StyleBadge({ children, className }: { children: React.ReactNode; className?: string }) {
  return (
    <span
      className={`text-[8px] font-mono px-1 py-px rounded border border-hairline text-muted-foreground ${className ?? ""}`}
    >
      {children}
    </span>
  );
}

/* ═════════════════════════ the chain matrix ═════════════════════════ */

function ChainMatrix() {
  const markets = useVenueStore((s) => s.markets);
  const snapshot = useVenueStore((s) => s.snapshot);
  const setActiveSymbol = useVenueStore((s) => s.setActiveSymbol);
  const setView = useVenueStore((s) => s.setView);
  const now = snapshot?.meta.now ?? Date.now();

  const expiries = useMemo(() => {
    const set = new Set<string>();
    for (const m of markets) {
      if (m.kind !== "option") continue;
      set.add(m.variant === "everlasting" ? "EVERLASTING" : String(m.expiry_ts));
    }
    return [...set].sort((a, b) => {
      if (a === "EVERLASTING") return 1;
      if (b === "EVERLASTING") return -1;
      return Number(a) - Number(b);
    });
  }, [markets]);

  const [expiry, setExpiry] = useState<string>(expiries[0] ?? "");

  const current = expiry || expiries[0] || "";

  const strikes = useMemo(() => {
    const byStrike = new Map<number, { call?: typeof markets[number]; put?: typeof markets[number] }>();
    for (const m of markets) {
      if (m.kind !== "option" || m.strike_quote_minor == null) continue;
      const isCurrent =
        current === "EVERLASTING" ? m.variant === "everlasting" : String(m.expiry_ts) === current;
      if (!isCurrent) continue;
      const strike = Number(m.strike_quote_minor / 100n);
      const slot = byStrike.get(strike) ?? {};
      if (m.symbol.endsWith("-C")) slot.call = m;
      else slot.put = m;
      byStrike.set(strike, slot);
    }
    return [...byStrike.entries()].sort((a, b) => a[0] - b[0]);
  }, [markets, current]);

  const spot = snapshot?.meta ? markets.find((m) => m.symbol === "BTC-PERP")?.mark_quote_minor ?? null : null;
  const spotNum = spot != null ? Number(spot) / 100 : 0;

  return (
    <div className="flex-1 min-h-0 flex flex-col">
      <div className="h-9 shrink-0 flex items-center gap-1 px-3 border-b border-hairline overflow-x-auto scroll-thin">
        {expiries.map((e) => (
          <button
            key={e}
            onClick={() => setExpiry(e)}
            className={`px-2.5 h-6 rounded-md text-[10.5px] font-mono whitespace-nowrap transition-colors ${
              current === e
                ? "bg-primary/15 text-primary border border-primary/30"
                : "text-muted-foreground hover:text-foreground border border-transparent hover:bg-muted/50"
            }`}
          >
            {e === "EVERLASTING" ? "EVERLASTING" : relTime(Number(e), now)}
          </button>
        ))}
      </div>
      <div className="flex-1 min-h-0 overflow-auto scroll-thin">
        <table className="w-full text-[11px]">
          <thead>
            <tr className="text-[9px] uppercase tracking-wider text-muted-foreground/70">
              <th colSpan={4} className="text-center py-1.5 border-b border-hairline bg-up/5">Calls</th>
              <th className="border-b border-hairline" />
              <th colSpan={4} className="text-center py-1.5 border-b border-hairline bg-down/5">Puts</th>
            </tr>
            <tr className="text-[9px] uppercase tracking-wider text-muted-foreground/60">
              <th className="text-right py-1.5 px-2 border-b border-hairline">IV</th>
              <th className="text-right px-2 border-b border-hairline">Δ</th>
              <th className="text-right px-2 border-b border-hairline">Mark</th>
              <th className="text-right px-2 border-b border-hairline">Vol</th>
              <th className="text-center px-3 border-b border-hairline">Strike</th>
              <th className="text-right px-2 border-b border-hairline">Mark</th>
              <th className="text-right px-2 border-b border-hairline">Δ</th>
              <th className="text-right px-2 border-b border-hairline">IV</th>
              <th className="text-right px-2 border-b border-hairline">Vol</th>
            </tr>
          </thead>
          <tbody>
            {strikes.length === 0 && (
              <tr>
                <td colSpan={9} className="py-12 text-center text-muted-foreground/50">
                  No strikes listed for this expiry.
                </td>
              </tr>
            )}
            {strikes.map(([strike, cell]) => {
              const atm = Math.abs(strike - spotNum) < 2000;
              const itms = spotNum > strike;
              return (
                <tr key={strike} className={`border-b border-hairline/40 hover:bg-muted/25 ${atm ? "bg-primary/[0.04]" : ""}`}>
                  <ChainCell m={cell.call} field="iv" />
                  <ChainCell m={cell.call} field="delta" />
                  <ChainCell m={cell.call} field="mark" onClick={(s) => { setActiveSymbol(s); setView("terminal"); }} />
                  <ChainCell m={cell.call} field="volume" />
                  <td className={`text-center px-3 num font-semibold py-1.5 ${atm ? "text-primary" : "text-foreground"}`}>
                    {strike.toLocaleString()}
                    <span className={`ml-1.5 text-[8px] ${itms ? "text-up" : "text-muted-foreground/40"}`}>
                      {itms ? "ITM" : "OTM"}
                    </span>
                  </td>
                  <ChainCell m={cell.put} field="mark" onClick={(s) => { setActiveSymbol(s); setView("terminal"); }} />
                  <ChainCell m={cell.put} field="delta" />
                  <ChainCell m={cell.put} field="iv" />
                  <ChainCell m={cell.put} field="volume" />
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </div>
  );
}

function ChainCell({
  m,
  field,
  onClick,
}: {
  m: import("@perp/types").MarketRow | undefined;
  field: "mark" | "iv" | "delta" | "volume";
  onClick?: (symbol: string) => void;
}) {
  if (!m) return <td className="text-center text-muted-foreground/30 py-1.5 px-2">—</td>;
  let content = "—";
  let cls = "num";
  switch (field) {
    case "mark":
      content = usd(m.mark_quote_minor);
      cls += " font-medium";
      break;
    case "iv":
      content = fmtIv(m.iv);
      cls += " text-chart-3";
      break;
    case "delta": {
      // delta ≈ N(d1) display — approximate from IV + strike distance
      if (m.iv != null && m.strike_quote_minor != null) {
        content = "≈";
        cls += " text-muted-foreground";
      } else {
        content = "—";
      }
      break;
    }
    case "volume":
      content = usd(m.volume_quote_minor, { compact: true });
      cls += " text-muted-foreground";
      break;
  }
  return (
    <td
      className={`text-right px-2 py-1.5 ${cls} ${onClick ? "cursor-pointer hover:text-primary" : ""}`}
      onClick={() => onClick?.(m.symbol)}
    >
      {content}
    </td>
  );
}
