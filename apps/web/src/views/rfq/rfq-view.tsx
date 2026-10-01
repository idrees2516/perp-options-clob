"use client";

/**
 * RFQ — the dealer liquidity desk (G-11/14/38).
 *
 * Multi-leg packages routed to private directed counterparties: firm maker
 * quotes with TTL, atomic execution through the full margin + fee path,
 * grouped multi-leg discounts (cheapest group free, next two at 50%),
 * plus block trades that print to the tape on a 15-minute delay.
 */

import { Fragment, memo, useMemo, useState } from "react";
import { useVenueStore } from "@/lib/venue-store";
import { getVenueClient } from "@perp/api-client";
import { usd, relTime, simTime, shortSymbol, sizeBase, ticksToPrice } from "@/lib/fmt";
import { tryParseMoney } from "@perp/types";
import type { Instrument, RfqLegCommand } from "@perp/types";
import type { RfqView as RfqRow, RfqQuoteView } from "@perp/types";
import { toast } from "sonner";
import { cn } from "@/lib/utils";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectLabel,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  MessagesSquare,
  Info,
  Plus,
  X,
  ChevronDown,
  Zap,
  Ban,
  ArrowRight,
} from "lucide-react";

const TTL_OPTIONS = [
  { value: "10", label: "10s" },
  { value: "30", label: "30s" },
  { value: "60", label: "60s" },
];

export const RfqView = memo(function RfqView() {
  const snapshot = useVenueStore((s) => s.snapshot);
  const account = useVenueStore((s) => s.activeAccount);

  const now = snapshot?.meta.now ?? Date.now();
  const descriptors = snapshot?.accountDescriptors ?? [];

  const [expanded, setExpanded] = useState<number | null>(null);

  /* ── builder state ── */
  const [legs, setLegs] = useState<RfqLegCommand[]>([]);
  const [symbol, setSymbol] = useState("BTC-PERP");
  const [side, setSide] = useState<"bid" | "ask">("bid");
  const [qty, setQty] = useState("5");
  const [minCost, setMinCost] = useState("");
  const [maxCost, setMaxCost] = useState("");
  const [ttl, setTtl] = useState("30");

  const instMap = useMemo(() => {
    const m = new Map<string, Instrument>();
    for (const i of snapshot?.instruments ?? []) m.set(i.symbol, i);
    return m;
  }, [snapshot]);

  const nameFor = (id: number): string =>
    descriptors.find((d) => d.id === id)?.name ?? `Sub ${id}`;
  const colorFor = (id: number): string =>
    descriptors.find((d) => d.id === id)?.color ?? "#888";

  /** Instruments for the picker: perps first, options grouped by expiry. */
  const symbolGroups = useMemo(() => {
    const groups: { label: string; items: Instrument[] }[] = [];
    const byExpiry = new Map<string, Instrument[]>();
    for (const inst of snapshot?.instruments ?? []) {
      if (inst.kind === "perp") {
        const g = groups.find((x) => x.label === "Perp");
        if (g) g.items.push(inst);
        else groups.push({ label: "Perp", items: [inst] });
      } else {
        const label =
          inst.variant === "everlasting" ? "Everlasting" : simTimeOfExpiry(inst);
        const list = byExpiry.get(label) ?? [];
        list.push(inst);
        byExpiry.set(label, list);
      }
    }
    for (const [label, items] of byExpiry) {
      groups.push({
        label,
        items: items.sort((a, b) =>
          a.symbol === b.symbol ? 0 : a.symbol.localeCompare(b.symbol),
        ),
      });
    }
    return groups;
  }, [snapshot]);

  const openRfqs = useMemo(
    () => (snapshot?.rfqs ?? []).filter((r) => r.status === "open" || r.status === "quoted"),
    [snapshot],
  );
  const history = useMemo(
    () =>
      (snapshot?.rfqs ?? [])
        .filter((r) => r.status === "executed" || r.status === "expired" || r.status === "cancelled")
        .slice(-30)
        .reverse(),
    [snapshot],
  );
  const blocks = useMemo(
    () => (snapshot?.blocks ?? []).slice(-20).reverse(),
    [snapshot],
  );

  /* ── builder actions ── */

  const addLeg = () => {
    const inst = instMap.get(symbol) ?? null;
    const q = Math.floor(Number(qty));
    if (!inst) {
      toast.error("Unknown instrument", { description: "Pick an instrument from the chain first." });
      return;
    }
    if (!Number.isFinite(q) || q <= 0) {
      toast.error("Invalid quantity", { description: "Leg size must be a positive number of lots." });
      return;
    }
    if (q > inst.max_order_lots) {
      toast.error("Above max order size", {
        description: `${inst.symbol} caps at ${inst.max_order_lots.toLocaleString("en-US")} lots.`,
      });
      return;
    }
    setLegs((l) => [...l, { symbol: inst.symbol, side, qty_lots: q }]);
  };

  const submit = () => {
    if (legs.length === 0) {
      toast.error("Add at least one leg", {
        description: "A package needs one or more legs before dealers can quote it.",
      });
      return;
    }
    const min = minCost.trim() ? tryParseMoney(minCost, 2) : null;
    const max = maxCost.trim() ? tryParseMoney(maxCost, 2) : null;
    if (min != null && min < 0n) {
      toast.error("Invalid min bound", { description: "Min total cost must be a positive USD amount." });
      return;
    }
    if (max != null && max < 0n) {
      toast.error("Invalid max bound", { description: "Max total cost must be a positive USD amount." });
      return;
    }
    if (min != null && max != null && min > max) {
      toast.error("Bounds inverted", { description: "Min total cost is above max total cost." });
      return;
    }
    getVenueClient().send({
      type: "command",
      command: {
        type: "rfq_create",
        taker: account,
        legs,
        counterparties: [1, 2],
        min_total_cost_quote_minor: min != null && min > 0n ? min : null,
        max_total_cost_quote_minor: max != null && max > 0n ? max : null,
        ttl_ms: Number(ttl) * 1000,
        now,
      },
    });
    toast.success(`RFQ out to ${legs.length} leg${legs.length > 1 ? "s" : ""}`, {
      description: "Command::RfqCreate → dealers quoting…",
    });
    setLegs([]);
  };

  const executeQuote = (rfq: RfqRow, quoteId: number) => {
    getVenueClient().send({
      type: "command",
      command: { type: "rfq_execute", taker: account, rfq_id: rfq.rfq_id, quote_id: quoteId, now },
    });
    toast.success(`Executing package #${rfq.rfq_id}`, {
      description: `Command::RfqExecute → quote #${quoteId} → Event::RfqSettled (atomic · margin + fees)`,
    });
    setExpanded(null);
  };

  const cancelRfq = (rfq: RfqRow) => {
    getVenueClient().send({
      type: "command",
      command: { type: "rfq_cancel", subaccount: account, rfq_id: rfq.rfq_id, quote_id: null, now },
    });
    toast.success(`RFQ #${rfq.rfq_id} cancelled`, {
      description: "Command::RfqCancel → Event::RfqClosed",
    });
    setExpanded(null);
  };

  return (
    <div className="h-full flex flex-col min-h-0">
      {/* Toolbar */}
      <div className="h-11 shrink-0 flex items-center gap-2.5 px-3 border-b border-hairline">
        <MessagesSquare className="w-4 h-4 text-primary" />
        <h1 className="text-[13px] font-semibold">Dealer Liquidity Desk</h1>
        <span className="text-[8px] font-mono px-1.5 py-0.5 rounded border border-primary/30 bg-primary/10 text-primary">
          G-11/14/38
        </span>
        <div className="flex-1" />
        <p className="text-[10px] text-muted-foreground/60 font-mono hidden md:block truncate">
          {openRfqs.length} live · sim MMs auto-quote in ~1s · scripted RFQs every ~90s
        </p>
      </div>

      <div className="flex-1 min-h-0 overflow-auto scroll-thin p-3">
        <div className="grid gap-3 xl:grid-cols-[380px_minmax(0,1fr)] items-start">
          {/* ── Builder ── */}
          <section className="panel p-4 flex flex-col gap-3" aria-label="Create RFQ">
            <div>
              <h2 className="text-[12.5px] font-semibold leading-none">Create RFQ</h2>
              <p className="text-[9px] text-muted-foreground mt-1">
                Multi-leg package · private directed counterparties (MM-1 · MM-2)
              </p>
            </div>

            {/* Legs */}
            <div className="space-y-1.5">
              {legs.length === 0 ? (
                <p className="text-[10.5px] text-muted-foreground/50 py-2.5 text-center rounded-md border border-dashed border-hairline">
                  No legs yet — build the package below.
                </p>
              ) : (
                legs.map((l, i) => {
                  const inst = instMap.get(l.symbol) ?? null;
                  return (
                    <div
                      key={`${l.symbol}-${i}`}
                      className="flex items-center gap-2 rounded-md border border-hairline bg-muted/25 px-2 py-1.5"
                    >
                      <span className="text-[9px] font-mono text-muted-foreground/60 w-4">{i + 1}</span>
                      <span className="font-mono text-[11px] truncate">{shortSymbol(l.symbol)}</span>
                      <span
                        className={cn(
                          "text-[9px] font-mono px-1.5 py-px rounded",
                          l.side === "bid" ? "text-up bg-up/10" : "text-down bg-down/10",
                        )}
                      >
                        {l.side === "bid" ? "BUY" : "SELL"}
                      </span>
                      <span className="num text-[11px] ml-auto">
                        {l.qty_lots} {l.qty_lots === 1 ? "lot" : "lots"}
                      </span>
                      <span className="text-[9px] text-muted-foreground/70 hidden sm:inline">
                        {inst ? sizeBase(l.qty_lots, inst) : ""}
                      </span>
                      <button
                        onClick={() => setLegs((ls) => ls.filter((_, j) => j !== i))}
                        className="w-5 h-5 rounded flex items-center justify-center text-muted-foreground hover:text-down hover:bg-down/10 transition-all shrink-0"
                        aria-label={`Remove leg ${i + 1}`}
                      >
                        <X className="w-3 h-3" />
                      </button>
                    </div>
                  );
                })
              )}
            </div>

            {/* Add-leg controls */}
            <div className="grid grid-cols-2 gap-2">
              <div className="col-span-2 space-y-1">
                <label
                  htmlFor="rfq-symbol"
                  className="text-[9px] uppercase tracking-wider text-muted-foreground/70 block"
                >
                  Instrument
                </label>
                <Select value={symbol} onValueChange={setSymbol}>
                  <SelectTrigger id="rfq-symbol" size="sm" className="w-full h-8 text-[11px] font-mono bg-transparent border-hairline">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent className="max-h-72">
                    {symbolGroups.map((g) => (
                      <SelectGroup key={g.label}>
                        <SelectLabel className="text-[9px] uppercase tracking-wider">{g.label}</SelectLabel>
                        {g.items.map((i) => (
                          <SelectItem key={i.symbol} value={i.symbol} className="text-[11px] font-mono">
                            {shortSymbol(i.symbol)}
                          </SelectItem>
                        ))}
                      </SelectGroup>
                    ))}
                  </SelectContent>
                </Select>
              </div>
              <div className="space-y-1">
                <span className="text-[9px] uppercase tracking-wider text-muted-foreground/70 block">
                  Side
                </span>
                <div className="grid grid-cols-2 gap-1" role="group" aria-label="Leg side">
                  <button
                    onClick={() => setSide("bid")}
                    aria-pressed={side === "bid"}
                    className={cn(
                      "h-8 rounded-md text-[11px] font-semibold transition-all focus-glow",
                      side === "bid"
                        ? "bg-up/15 text-up border border-up/40"
                        : "bg-muted/40 text-muted-foreground hover:text-foreground border border-transparent",
                    )}
                  >
                    Buy
                  </button>
                  <button
                    onClick={() => setSide("ask")}
                    aria-pressed={side === "ask"}
                    className={cn(
                      "h-8 rounded-md text-[11px] font-semibold transition-all focus-glow",
                      side === "ask"
                        ? "bg-down/15 text-down border border-down/40"
                        : "bg-muted/40 text-muted-foreground hover:text-foreground border border-transparent",
                    )}
                  >
                    Sell
                  </button>
                </div>
              </div>
              <div className="space-y-1">
                <label
                  htmlFor="rfq-qty"
                  className="text-[9px] uppercase tracking-wider text-muted-foreground/70 block"
                >
                  Qty (lots)
                </label>
                <Input
                  id="rfq-qty"
                  value={qty}
                  onChange={(e) => setQty(e.target.value)}
                  inputMode="numeric"
                  className="h-8 num text-[11.5px]"
                  placeholder="5"
                />
              </div>
              <Button
                size="sm"
                variant="outline"
                onClick={addLeg}
                className="col-span-2 h-8 text-[11px] border-primary/40 text-primary hover:bg-primary/10 hover:text-primary"
              >
                <Plus className="w-3.5 h-3.5 mr-1" /> Add leg
              </Button>
            </div>

            {/* Bounds + TTL */}
            <div className="grid grid-cols-3 gap-2">
              <div className="space-y-1">
                <label
                  htmlFor="rfq-min"
                  className="text-[9px] uppercase tracking-wider text-muted-foreground/70 block"
                >
                  Min cost $
                </label>
                <Input
                  id="rfq-min"
                  value={minCost}
                  onChange={(e) => setMinCost(e.target.value)}
                  inputMode="decimal"
                  placeholder="optional"
                  className="h-8 num text-[11.5px]"
                />
              </div>
              <div className="space-y-1">
                <label
                  htmlFor="rfq-max"
                  className="text-[9px] uppercase tracking-wider text-muted-foreground/70 block"
                >
                  Max cost $
                </label>
                <Input
                  id="rfq-max"
                  value={maxCost}
                  onChange={(e) => setMaxCost(e.target.value)}
                  inputMode="decimal"
                  placeholder="optional"
                  className="h-8 num text-[11.5px]"
                />
              </div>
              <div className="space-y-1">
                <span className="text-[9px] uppercase tracking-wider text-muted-foreground/70 block">
                  TTL
                </span>
                <Select value={ttl} onValueChange={setTtl}>
                  <SelectTrigger size="sm" className="w-full h-8 text-[11px] bg-transparent border-hairline">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {TTL_OPTIONS.map((o) => (
                      <SelectItem key={o.value} value={o.value} className="text-xs">
                        {o.label}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </div>
            </div>

            <Button
              onClick={submit}
              className="h-10 w-full font-semibold text-[13px] bg-primary/90 hover:bg-primary text-primary-foreground"
            >
              Send RFQ · {legs.length || 0} leg{legs.length === 1 ? "" : "s"} · {ttl}s
            </Button>
            <p className="text-[9px] text-muted-foreground/50 text-center font-mono leading-relaxed">
              Command::RfqCreate → Event::RfqCreated → RfqQuoted → atomic execute
            </p>
          </section>

          {/* ── Right column ── */}
          <div className="flex flex-col gap-3 min-w-0">
            {/* Open RFQs */}
            <section className="panel" aria-label="Open RFQs">
              <header className="h-9 px-3 flex items-center justify-between border-b border-hairline">
                <h2 className="text-[10px] uppercase tracking-wider text-muted-foreground font-medium">
                  Open RFQs
                </h2>
                <span className="text-[9px] font-mono text-muted-foreground/50">
                  {openRfqs.length} live · {openRfqs.filter((r) => r.quotes.length > 0).length} quoted
                </span>
              </header>
              <div className="max-h-[24rem] overflow-auto scroll-thin">
                <table className="w-full text-[11.5px] border-collapse">
                  <thead>
                    <tr>
                      <Th>RFQ</Th>
                      <Th>Taker</Th>
                      <Th>Package</Th>
                      <Th right>Quotes</Th>
                      <Th right>Best total cost</Th>
                      <Th right>Expires</Th>
                      <Th>Status</Th>
                      <Th />
                    </tr>
                  </thead>
                  <tbody>
                    {openRfqs.length === 0 && (
                      <EmptyRow cols={8} text="No open RFQs — the dealer desk is quiet." />
                    )}
                    {openRfqs.map((r) => {
                      const best = bestQuote(r);
                      const isOpen = expanded === r.rfq_id;
                      const mine = r.taker === account;
                      return (
                        <Fragment key={r.rfq_id}>
                          <tr
                            className={cn(
                              "hover:bg-muted/30 border-b border-hairline/40",
                              isOpen && "bg-muted/30",
                            )}
                          >
                            <Td className="font-mono text-[11px]">#{r.rfq_id}</Td>
                            <Td>
                              <span className="flex items-center gap-1.5">
                                <span
                                  className="w-1.5 h-1.5 rounded-full shrink-0"
                                  style={{ background: colorFor(r.taker) }}
                                />
                                <span className="font-medium">{nameFor(r.taker)}</span>
                                {mine && (
                                  <span className="text-[8px] text-primary font-mono">you</span>
                                )}
                              </span>
                            </Td>
                            <Td>
                              <span className="flex flex-wrap gap-x-2.5 gap-y-0.5">
                                {r.legs.map((l, i) => (
                                  <span key={i} className="inline-flex items-baseline gap-1 whitespace-nowrap">
                                    <span className={l.side === "bid" ? "text-up" : "text-down"}>
                                      {l.side === "bid" ? "B" : "S"}
                                    </span>
                                    <span className="font-mono text-[11px]">{shortSymbol(l.symbol)}</span>
                                    <span className="num text-[10px] text-muted-foreground">{l.qty_lots}</span>
                                  </span>
                                ))}
                              </span>
                            </Td>
                            <Td right className={r.quotes.length > 0 ? "text-primary" : "text-muted-foreground/50"}>
                              {r.quotes.length}
                            </Td>
                            <Td right>
                              {best ? (
                                <span className={costClass(best.total_cost_quote_minor)}>
                                  {usd(best.total_cost_quote_minor, { sign: true })}
                                </span>
                              ) : (
                                <span className="text-muted-foreground/50">—</span>
                              )}
                            </Td>
                            <Td right className="text-muted-foreground">{relTime(r.expires_at, now)}</Td>
                            <Td>
                              <StatusBadge status={r.status} />
                            </Td>
                            <Td>
                              <button
                                onClick={() => setExpanded(isOpen ? null : r.rfq_id)}
                                aria-label={isOpen ? `Collapse quotes for RFQ ${r.rfq_id}` : `Expand quotes for RFQ ${r.rfq_id}`}
                                aria-expanded={isOpen}
                                className="w-5 h-5 rounded flex items-center justify-center text-muted-foreground hover:text-foreground hover:bg-muted transition-all"
                              >
                                <ChevronDown
                                  className={cn("w-3 h-3 transition-transform", isOpen && "rotate-180")}
                                />
                              </button>
                            </Td>
                          </tr>
                          {isOpen && (
                            <tr className="border-b border-hairline/40">
                              <td colSpan={8} className="p-0">
                                <QuotesPanel
                                  rfq={r}
                                  instMap={instMap}
                                  nameFor={nameFor}
                                  now={now}
                                  canAct={mine && (r.status === "open" || r.status === "quoted")}
                                  onExecute={executeQuote}
                                  onCancel={cancelRfq}
                                />
                              </td>
                            </tr>
                          )}
                        </Fragment>
                      );
                    })}
                  </tbody>
                </table>
              </div>
            </section>

            {/* History + blocks */}
            <div className="grid gap-3 lg:grid-cols-2 items-start">
              <section className="panel" aria-label="RFQ history">
                <header className="h-9 px-3 flex items-center justify-between border-b border-hairline">
                  <h2 className="text-[10px] uppercase tracking-wider text-muted-foreground font-medium">
                    History
                  </h2>
                  <span className="text-[9px] font-mono text-muted-foreground/50">
                    executed · expired · cancelled
                  </span>
                </header>
                <div className="max-h-72 overflow-auto scroll-thin">
                  <table className="w-full text-[11.5px] border-collapse">
                    <thead>
                      <tr>
                        <Th>RFQ</Th>
                        <Th>Taker</Th>
                        <Th>Package</Th>
                        <Th>Status</Th>
                        <Th>Trades</Th>
                        <Th right>Settled</Th>
                      </tr>
                    </thead>
                    <tbody>
                      {history.length === 0 && (
                        <EmptyRow cols={6} text="No history yet — RFQs land here after execution or expiry." />
                      )}
                      {history.map((r) => (
                        <tr key={r.rfq_id} className="hover:bg-muted/30 border-b border-hairline/40 align-top">
                          <Td className="font-mono text-[11px]">#{r.rfq_id}</Td>
                          <Td>{nameFor(r.taker)}</Td>
                          <Td>
                            <span className="flex flex-wrap gap-x-2 gap-y-0.5">
                              {r.legs.map((l, i) => (
                                <span key={i} className="inline-flex items-baseline gap-1 whitespace-nowrap">
                                  <span className={l.side === "bid" ? "text-up" : "text-down"}>
                                    {l.side === "bid" ? "B" : "S"}
                                  </span>
                                  <span className="font-mono text-[11px]">{shortSymbol(l.symbol)}</span>
                                  <span className="num text-[10px] text-muted-foreground">{l.qty_lots}</span>
                                </span>
                              ))}
                            </span>
                          </Td>
                          <Td>
                            <StatusBadge status={r.status} />
                          </Td>
                          <Td>
                            {r.trades && r.trades.length > 0 ? (
                              <span className="flex flex-col gap-0.5">
                                {r.trades.map((t, i) => (
                                  <span key={i} className="num text-[10.5px] whitespace-nowrap">
                                    {shortSymbol(t.symbol)} {t.qty_lots} @{" "}
                                    {ticksToPrice(t.price_ticks, instMap.get(t.symbol))}
                                  </span>
                                ))}
                              </span>
                            ) : (
                              <span className="text-muted-foreground/50">—</span>
                            )}
                          </Td>
                          <Td right className="text-muted-foreground/60 text-[10px] font-mono">
                            {simTime(r.trades?.[0]?.ts ?? r.expires_at)}
                          </Td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              </section>

              <section className="panel" aria-label="Block trades">
                <header className="h-9 px-3 flex items-center justify-between border-b border-hairline">
                  <h2 className="text-[10px] uppercase tracking-wider text-muted-foreground font-medium">
                    Block trades
                  </h2>
                  <span className="text-[9px] font-mono text-muted-foreground/50">
                    15-min delayed tape print
                  </span>
                </header>
                <div className="max-h-72 overflow-auto scroll-thin">
                  <table className="w-full text-[11.5px] border-collapse">
                    <thead>
                      <tr>
                        <Th>Block</Th>
                        <Th>Taker → Maker</Th>
                        <Th>Legs</Th>
                        <Th right>Notional</Th>
                        <Th right>Broadcast</Th>
                      </tr>
                    </thead>
                    <tbody>
                      {blocks.length === 0 && (
                        <EmptyRow cols={5} text="No blocks registered — the tape prints what dealers agree." />
                      )}
                      {blocks.map((b) => (
                        <tr key={b.block_id} className="hover:bg-muted/30 border-b border-hairline/40">
                          <Td className="font-mono text-[11px]">#{b.block_id}</Td>
                          <Td>
                            <span className="flex items-center gap-1 whitespace-nowrap">
                              {nameFor(b.taker)}
                              <ArrowRight className="w-3 h-3 text-muted-foreground/60" />
                              {nameFor(b.maker)}
                            </span>
                          </Td>
                          <Td>
                            <span className="flex flex-wrap gap-x-2 gap-y-0.5">
                              {b.legs.map((l, i) => (
                                <span key={i} className="inline-flex items-baseline gap-1 whitespace-nowrap">
                                  <span className={l.side === "bid" ? "text-up" : "text-down"}>
                                    {l.side === "bid" ? "B" : "S"}
                                  </span>
                                  <span className="font-mono text-[11px]">{shortSymbol(l.symbol)}</span>
                                  <span className="num text-[10px] text-muted-foreground">
                                    {l.qty_lots} @ {ticksToPrice(l.price_ticks, instMap.get(l.symbol))}
                                  </span>
                                </span>
                              ))}
                            </span>
                          </Td>
                          <Td right>{usd(b.total_notional_quote_minor)}</Td>
                          <Td right>
                            {b.printed ? (
                              <span className="text-[8px] font-mono px-1.5 py-0.5 rounded border border-up/40 bg-up/10 text-up">
                                PRINTED
                              </span>
                            ) : (
                              <span className="text-muted-foreground text-[10.5px]">
                                prints {relTime(b.broadcast_ts, now)}
                              </span>
                            )}
                          </Td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              </section>
            </div>
          </div>
        </div>

        {/* ── Explainer ── */}
        <section className="panel mt-3 p-4" aria-label="How the RFQ desk works">
          <div className="flex items-center gap-2 mb-1.5">
            <Info className="w-3.5 h-3.5 text-primary" />
            <h2 className="text-[10px] uppercase tracking-wider text-muted-foreground font-medium">
              How the RFQ desk works
            </h2>
          </div>
          <p className="text-[11px] leading-relaxed text-muted-foreground">
            Multi-leg packages, private directed counterparties, firm maker quotes with TTL, atomic
            execution through the full margin + fee path — grouped multi-leg discounts: cheapest
            group free, next two at 50%.
          </p>
        </section>
      </div>
    </div>
  );
});

/* ═════════════════════════ quotes panel ═════════════════════════ */

function QuotesPanel({
  rfq,
  instMap,
  nameFor,
  now,
  canAct,
  onExecute,
  onCancel,
}: {
  rfq: RfqRow;
  instMap: Map<string, Instrument>;
  nameFor: (id: number) => string;
  now: number;
  canAct: boolean;
  onExecute: (rfq: RfqRow, quoteId: number) => void;
  onCancel: (rfq: RfqRow) => void;
}) {
  return (
    <div className="px-3 py-2.5 bg-muted/15">
      <div className="flex items-start justify-between gap-3 flex-wrap mb-2">
        <p className="text-[9px] uppercase tracking-wider text-muted-foreground">
          Firm quotes · directed to{" "}
          <span className="font-mono text-muted-foreground/80">
            {rfq.counterparties.map((c) => nameFor(c)).join(" · ")}
          </span>{" "}
          · bounds{" "}
          <span className="num text-muted-foreground/80">
            {rfq.min_total_cost_quote_minor != null ? usd(rfq.min_total_cost_quote_minor) : "—"}/
            {rfq.max_total_cost_quote_minor != null ? usd(rfq.max_total_cost_quote_minor) : "—"}
          </span>
        </p>
        {canAct && (
          <Button
            size="sm"
            variant="outline"
            onClick={() => onCancel(rfq)}
            className="h-6 text-[10px] text-muted-foreground hover:text-down hover:border-down/40"
          >
            <Ban className="w-3 h-3 mr-1" /> Cancel RFQ
          </Button>
        )}
      </div>
      <div className="overflow-x-auto scroll-thin rounded-md border border-hairline">
        <table className="w-full text-[11px] border-collapse bg-panel">
          <thead>
            <tr>
              <Th flat>Maker</Th>
              <Th flat>Per-leg prices</Th>
              <Th flat right>Total cost</Th>
              <Th flat right>Quote expiry</Th>
              <Th flat />
            </tr>
          </thead>
          <tbody>
            {rfq.quotes.length === 0 && (
              <EmptyRow cols={5} text="No quotes yet — dealers respond within ~1s." />
            )}
            {rfq.quotes.map((q) => (
              <QuoteRow
                key={q.quote_id}
                quote={q}
                rfq={rfq}
                instMap={instMap}
                nameFor={nameFor}
                now={now}
                canExecute={canAct}
                onExecute={onExecute}
              />
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}

function QuoteRow({
  quote,
  rfq,
  instMap,
  nameFor,
  now,
  canExecute,
  onExecute,
}: {
  quote: RfqQuoteView;
  rfq: RfqRow;
  instMap: Map<string, Instrument>;
  nameFor: (id: number) => string;
  now: number;
  canExecute: boolean;
  onExecute: (rfq: RfqRow, quoteId: number) => void;
}) {
  const expiredQuote = quote.expires_at < now;
  return (
    <tr className={cn("hover:bg-muted/30 border-b border-hairline/40", expiredQuote && "opacity-60")}>
      <Td className="font-medium">{nameFor(quote.maker)}</Td>
      <Td>
        <span className="flex flex-wrap gap-x-3 gap-y-0.5">
          {quote.leg_prices_ticks.map((ticks, i) => {
            const leg = rfq.legs[i];
            if (!leg) return null;
            return (
              <span key={i} className="inline-flex items-baseline gap-1 whitespace-nowrap">
                <span className="font-mono text-[10.5px] text-muted-foreground">
                  {shortSymbol(leg.symbol)}
                </span>
                <span className="num text-[11px]">{ticksToPrice(ticks, instMap.get(leg.symbol))}</span>
              </span>
            );
          })}
        </span>
      </Td>
      <Td right className={costClass(quote.total_cost_quote_minor)}>
        {usd(quote.total_cost_quote_minor, { sign: true })}
      </Td>
      <Td right className="text-muted-foreground">{relTime(quote.expires_at, now)}</Td>
      <Td>
        {canExecute ? (
          <Button
            size="sm"
            onClick={() => onExecute(rfq, quote.quote_id)}
            className="h-6 text-[10px] bg-primary/90 hover:bg-primary"
          >
            <Zap className="w-3 h-3 mr-1" /> Execute
          </Button>
        ) : (
          <span className="text-[9px] text-muted-foreground/50">taker only</span>
        )}
      </Td>
    </tr>
  );
}

/* ═════════════════════════ locals ═════════════════════════ */

/** Cheapest quote by absolute total cost. */
function bestQuote(rfq: RfqRow): RfqQuoteView | null {
  let best: RfqQuoteView | null = null;
  let bestAbs: bigint | null = null;
  for (const q of rfq.quotes) {
    const abs = q.total_cost_quote_minor < 0n ? -q.total_cost_quote_minor : q.total_cost_quote_minor;
    if (bestAbs == null || abs < bestAbs) {
      bestAbs = abs;
      best = q;
    }
  }
  return best;
}

/** Net debit (amber) vs net credit (green). */
function costClass(minor: bigint): string {
  if (minor < 0n) return "text-up";
  if (minor > 0n) return "text-chart-3";
  return "text-muted-foreground";
}

function StatusBadge({ status }: { status: string }) {
  const map: Record<string, string> = {
    open: "border-hairline text-muted-foreground",
    quoted: "border-primary/40 bg-primary/10 text-primary",
    executed: "border-up/40 bg-up/10 text-up",
    expired: "border-hairline text-muted-foreground/70",
    cancelled: "border-down/40 bg-down/10 text-down",
  };
  return (
    <span
      className={cn(
        "text-[8px] font-mono px-1.5 py-0.5 rounded border",
        map[status] ?? "border-hairline text-muted-foreground",
      )}
    >
      {status.toUpperCase()}
    </span>
  );
}

function simTimeOfExpiry(inst: Instrument): string {
  if (inst.kind !== "option") return "Option";
  if (inst.variant === "everlasting") return "Everlasting";
  const d = new Date(inst.expiry_ts_ms);
  return d.toLocaleDateString("en-US", { month: "short", day: "numeric", timeZone: "UTC" });
}

function Th({ children, right, flat }: { children?: React.ReactNode; right?: boolean; flat?: boolean }) {
  return (
    <th
      className={cn(
        "bg-panel text-[9px] uppercase tracking-wider text-muted-foreground/70 font-medium py-1.5 px-2.5 border-b border-hairline whitespace-nowrap",
        !flat && "sticky top-0 z-10 backdrop-blur-sm",
        right ? "text-right" : "text-left",
      )}
    >
      {children}
    </th>
  );
}

function Td({ children, right, className }: { children: React.ReactNode; right?: boolean; className?: string }) {
  return (
    <td className={cn("py-1.5 px-2.5 whitespace-nowrap", right ? "text-right num" : "", className ?? "")}>
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
