"use client";

/**
 * Bottom tabs — positions, open orders, fills, greeks.
 * The active account's working state, live.
 */

import { memo, useMemo, useState } from "react";
import { useVenueStore } from "@/lib/venue-store";
import { getVenueClient } from "@perp/api-client";
import { usd, sizeBase, pnl, shortSymbol, ticksToPrice } from "@/lib/fmt";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Button } from "@/components/ui/button";
import { X } from "lucide-react";
import type { Instrument, OrderType, TimeInForce } from "@perp/types";

export const BottomTabs = memo(function BottomTabs() {
  const snapshot = useVenueStore((s) => s.snapshot);
  const account = useVenueStore((s) => s.activeAccount);
  const [tab, setTab] = useState("positions");

  const positions = snapshot?.positions[account] ?? [];
  const orders = snapshot?.openOrders[account] ?? [];
  const fills = useMemo(
    () => (snapshot?.fills ?? []).filter((f) => f.subaccount === account).slice(-120).reverse(),
    [snapshot, account],
  );
  const greeks = snapshot?.greeks[account] ?? [];
  const insts = useMemo(() => {
    const m = new Map<string, Instrument>();
    for (const i of snapshot?.instruments ?? []) m.set(i.symbol, i);
    return m;
  }, [snapshot]);

  return (
    <Tabs value={tab} onValueChange={setTab} className="h-full flex flex-col">
      <TabsList className="h-9 mx-3 mt-2 w-fit bg-muted/40 border border-hairline">
        <TabsTrigger value="positions" className="text-[11px] h-7 data-[state=active]:bg-primary/15">
          Positions
          <Count n={positions.length} />
        </TabsTrigger>
        <TabsTrigger value="orders" className="text-[11px] h-7 data-[state=active]:bg-primary/15">
          Orders
          <Count n={orders.length} />
        </TabsTrigger>
        <TabsTrigger value="fills" className="text-[11px] h-7 data-[state=active]:bg-primary/15">
          Fills
        </TabsTrigger>
        <TabsTrigger value="greeks" className="text-[11px] h-7 data-[state=active]:bg-primary/15">
          Greeks
        </TabsTrigger>
      </TabsList>
      <div className="flex-1 min-h-0 mx-3 mb-2 mt-1.5 rounded-lg border border-hairline overflow-hidden">
        <TabsContent value="positions" className="h-full m-0 data-[state=inactive]:hidden">
          <ScrollTable>
            <thead>
              <tr>
                <Th>Instrument</Th>
                <Th>Side</Th>
                <Th right>Size</Th>
                <Th right>Entry</Th>
                <Th right>Mark</Th>
                <Th right>Unrealized</Th>
                <Th right>Realized</Th>
                <Th right>Notional</Th>
              </tr>
            </thead>
            <tbody>
              {positions.length === 0 && <EmptyRow cols={8} text="No open positions — the book is flat." />}
              {positions.map((p) => {
                const inst = insts.get(p.symbol);
                const long = p.signed_lots > 0;
                return (
                  <tr key={p.symbol} className="hover:bg-muted/30">
                    <Td className="font-mono text-[11px]">{shortSymbol(p.symbol)}</Td>
                    <Td className={long ? "text-up" : "text-down"}>{long ? "Long" : "Short"}</Td>
                    <Td right>{inst ? sizeBase(p.signed_lots, inst) : p.signed_lots}</Td>
                    <Td right>{usd(p.avg_entry_quote_minor)}</Td>
                    <Td right>{usd(p.mark_quote_minor_per_base)}</Td>
                    <Td right className={p.unrealized_pnl_quote_minor >= 0n ? "text-up" : "text-down"}>
                      {pnl(p.unrealized_pnl_quote_minor)}
                    </Td>
                    <Td right className={p.realized_pnl_quote_minor >= 0n ? "text-up" : "text-down"}>
                      {pnl(p.realized_pnl_quote_minor)}
                    </Td>
                    <Td right className="text-muted-foreground">{usd(p.notional_quote_minor, { compact: true })}</Td>
                  </tr>
                );
              })}
            </tbody>
          </ScrollTable>
        </TabsContent>

        <TabsContent value="orders" className="h-full m-0 data-[state=inactive]:hidden">
          <div className="h-full flex flex-col">
            <div className="flex items-center justify-end gap-2 px-3 h-8 border-b border-hairline">
              <Button
                variant="ghost"
                size="sm"
                className="h-6 text-[10px] text-muted-foreground hover:text-down"
                onClick={() => {
                  getVenueClient().send({
                    type: "command",
                    command: { type: "cancel_all", subaccount: account, symbol: null, now: snapshot?.meta.now ?? Date.now() },
                  });
                }}
              >
                Cancel all
              </Button>
            </div>
            <div className="flex-1 min-h-0">
              <ScrollTable>
                <thead>
                  <tr>
                    <Th>Instrument</Th>
                    <Th>Side</Th>
                    <Th>Type</Th>
                    <Th right>Price</Th>
                    <Th right>Size</Th>
                    <Th right>Filled</Th>
                    <Th>TIF</Th>
                    <Th>Flags</Th>
                    <Th right>Placed</Th>
                    <Th />
                  </tr>
                </thead>
                <tbody>
                  {orders.length === 0 && <EmptyRow cols={10} text="No working orders." />}
                  {orders.map((o) => {
                    const inst = insts.get(o.symbol);
                    return (
                      <tr key={o.order_id} className="hover:bg-muted/30 group">
                        <Td className="font-mono text-[11px]">{shortSymbol(o.symbol)}</Td>
                        <Td className={o.side === "bid" ? "text-up" : "text-down"}>{o.side === "bid" ? "Buy" : "Sell"}</Td>
                        <Td className="text-muted-foreground">{o.order_type_label}</Td>
                        <Td right>{o.price_ticks != null ? ticksToPrice(o.price_ticks, inst ?? null) : "MKT"}</Td>
                        <Td right>{inst ? sizeBase(o.qty_lots, inst) : o.qty_lots}</Td>
                        <Td right>{o.filled_lots > 0 ? `${o.filled_lots}/${o.qty_lots}` : "—"}</Td>
                        <Td className="text-muted-foreground font-mono text-[10px]">{o.tif_label}</Td>
                        <Td>
                          <span className="flex gap-1">
                            {o.post_only && <Flag>POST</Flag>}
                            {o.reduce_only && <Flag>RO</Flag>}
                            {o.oco_group != null && <Flag>OCO</Flag>}
                          </span>
                        </Td>
                        <Td right className="text-muted-foreground/60 text-[10px] font-mono">
                          {new Date(o.ts).toISOString().slice(11, 19)}
                        </Td>
                        <Td>
                          <button
                            onClick={() =>
                              getVenueClient().send({
                                type: "command",
                                command: { type: "cancel", subaccount: account, order_id: o.order_id, now: snapshot?.meta.now ?? Date.now() },
                              })
                            }
                            className="opacity-0 group-hover:opacity-100 w-5 h-5 rounded flex items-center justify-center text-muted-foreground hover:text-down hover:bg-down/10 transition-all"
                            aria-label={`Cancel order ${o.order_id}`}
                          >
                            <X className="w-3 h-3" />
                          </button>
                        </Td>
                      </tr>
                    );
                  })}
                </tbody>
              </ScrollTable>
            </div>
          </div>
        </TabsContent>

        <TabsContent value="fills" className="h-full m-0 data-[state=inactive]:hidden">
          <ScrollTable>
            <thead>
              <tr>
                <Th>Instrument</Th>
                <Th>Role</Th>
                <Th>Side</Th>
                <Th right>Price</Th>
                <Th right>Size</Th>
                <Th right>Fee</Th>
                <Th right>Time</Th>
              </tr>
            </thead>
            <tbody>
              {fills.length === 0 && <EmptyRow cols={7} text="No fills yet." />}
              {fills.map((f, i) => {
                const inst = insts.get(f.symbol);
                return (
                  <tr key={`${f.seq}-${f.role}-${i}`} className="hover:bg-muted/30">
                    <Td className="font-mono text-[11px]">{shortSymbol(f.symbol)}</Td>
                    <Td>
                      <span className={f.liquidity_label === "T" ? "text-chart-3" : "text-primary"}>
                        {f.liquidity_label === "T" ? "Taker" : "Maker"}
                      </span>
                    </Td>
                    <Td className={f.maker_side === "bid" ? "text-up" : "text-down"}>
                      {f.maker_side === "bid" ? "Buy" : "Sell"}
                    </Td>
                    <Td right>{ticksToPrice(f.price_ticks, inst ?? null)}</Td>
                    <Td right>{inst ? sizeBase(f.qty_lots, inst) : f.qty_lots}</Td>
                    <Td right className={f.fee_quote_minor < 0n ? "text-up" : "text-muted-foreground"}>
                      {f.fee_quote_minor < 0n ? `+${usd(-f.fee_quote_minor)} rebate` : usd(f.fee_quote_minor)}
                    </Td>
                    <Td right className="text-muted-foreground/60 text-[10px] font-mono">
                      {new Date(f.ts).toISOString().slice(11, 19)}
                    </Td>
                  </tr>
                );
              })}
            </tbody>
          </ScrollTable>
        </TabsContent>

        <TabsContent value="greeks" className="h-full m-0 data-[state=inactive]:hidden">
          <ScrollTable>
            <thead>
              <tr>
                <Th>Instrument</Th>
                <Th right>Δ delta</Th>
                <Th right>Γ gamma</Th>
                <Th right>ν vega</Th>
                <Th right>Θ theta</Th>
              </tr>
            </thead>
            <tbody>
              {greeks.length === 0 && <EmptyRow cols={5} text="No greeks exposure — flat or perp-only." />}
              {greeks.map((g) => (
                <tr key={g.symbol} className="hover:bg-muted/30">
                  <Td className="font-mono text-[11px]">{shortSymbol(g.symbol)}</Td>
                  <Td right>{g.delta.toFixed(3)}</Td>
                  <Td right className="text-muted-foreground">{g.gamma.toFixed(5)}</Td>
                  <Td right className="text-muted-foreground">{g.vega.toFixed(2)}</Td>
                  <Td right className="text-muted-foreground">{g.theta.toFixed(3)}</Td>
                </tr>
              ))}
            </tbody>
          </ScrollTable>
        </TabsContent>
      </div>
    </Tabs>
  );
});

function Count({ n }: { n: number }) {
  if (n === 0) return null;
  return <span className="ml-1.5 text-[9px] font-mono text-muted-foreground bg-muted rounded-full px-1.5">{n}</span>;
}

function Flag({ children }: { children: React.ReactNode }) {
  return (
    <span className="text-[8px] font-mono px-1 py-px rounded bg-muted text-muted-foreground">{children}</span>
  );
}

function ScrollTable({ children }: { children: React.ReactNode }) {
  return (
    <div className="h-full overflow-auto scroll-thin">
      <table className="w-full text-[11.5px] border-collapse">{children}</table>
    </div>
  );
}

function Th({ children, right }: { children?: React.ReactNode; right?: boolean }) {
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
    <td
      className={`py-1.5 px-2.5 whitespace-nowrap ${right ? "text-right num" : ""} ${className ?? ""}`}
    >
      {children}
    </td>
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
