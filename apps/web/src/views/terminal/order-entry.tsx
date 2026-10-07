"use client";

/**
 * The ticket — every order type the engine accepts, in one form:
 * Limit / Market / Stop-Market / Stop-Limit / Trailing (mkt & lmt),
 * TIF GTC/IOC/FOK/GTD, post-only, reduce-only, iceberg, OCO brackets.
 * Submit builds an `OrderRequest` (the engine's own shape) and sends
 * `Command::Place`.
 */

import { memo, useCallback, useEffect, useMemo, useState } from "react";
import { useVenueStore, marketRowFor } from "@/lib/venue-store";
import { getVenueClient } from "@perp/api-client";
import { limitOrder } from "@perp/types";
import { orderMarginFor } from "@perp/sim-engine";
import { usd, sizeBase } from "@/lib/fmt";
import { toast } from "sonner";
import { cn } from "@/lib/utils";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { Info, Layers, Zap } from "lucide-react";

type OrderKind = "limit" | "market" | "stop_market" | "stop_limit" | "trailing_market" | "trailing_limit";
type TifKind = "gtc" | "ioc" | "fok" | "gtd";

/** Book-ladder → ticket price hand-off (module-level signal). */
let ticketPriceSignal: ((ticks: number) => void) | null = null;
export function setTicketPrice(ticks: number): void {
  ticketPriceSignal?.(ticks);
}

export const OrderEntry = memo(function OrderEntry() {
  const snapshot = useVenueStore((s) => s.snapshot);
  const markets = useVenueStore((s) => s.markets);
  const activeSymbol = useVenueStore((s) => s.activeSymbol);
  const account = useVenueStore((s) => s.activeAccount);

  const [side, setSide] = useState<"bid" | "ask">("bid");
  const [kind, setKind] = useState<OrderKind>("limit");
  const [price, setPrice] = useState("");
  const [qty, setQty] = useState("");
  const [tif, setTif] = useState<TifKind>("gtc");
  const [gtdMin, setGtdMin] = useState("60");
  const [trigger, setTrigger] = useState("");
  const [offset, setOffset] = useState("");
  const [postOnly, setPostOnly] = useState(false);
  const [reduceOnly, setReduceOnly] = useState(false);
  const [iceberg, setIceberg] = useState(false);
  const [icebergLots, setIcebergLots] = useState("");
  const [oco, setOco] = useState(false);
  const [ocoPrice, setOcoPrice] = useState("");
  const [submitting, setSubmitting] = useState(false);

  const inst = useMemo(
    () => snapshot?.instruments.find((i) => i.symbol === activeSymbol) ?? null,
    [snapshot, activeSymbol],
  );
  const row = marketRowFor(markets, snapshot, activeSymbol);

  // Book click → load price.
  useEffect(() => {
    ticketPriceSignal = (ticks: number) => {
      setKind((k) => (k === "market" ? "limit" : k));
      setPrice(String(ticks));
    };
    return () => {
      ticketPriceSignal = null;
    };
  }, []);

  const markTicks = useMemo(() => {
    if (!inst || !row?.mark_quote_minor) return null;
    return Number(row.mark_quote_minor / inst.tick_size_quote_minor);
  }, [inst, row]);

  const parsedQty = useMemo(() => {
    const n = Number(qty);
    return Number.isFinite(n) && n > 0 ? Math.floor(n) : null;
  }, [qty]);

  const parsedPriceTicks = useMemo(() => {
    if (kind === "market" || kind === "stop_market" || kind === "trailing_market") return null;
    const n = Number(price);
    return Number.isFinite(n) && n > 0 ? Math.floor(n) : null;
  }, [price, kind]);

  const estMargin = useMemo(() => {
    if (!inst || !parsedQty || markTicks == null) return null;
    const p = parsedPriceTicks ?? markTicks;
    return orderMarginFor(inst, parsedQty, p, row?.mark_quote_minor ?? 0n);
  }, [inst, parsedQty, parsedPriceTicks, markTicks, row]);

  const position = snapshot?.positions[account]?.find((p) => p.symbol === activeSymbol) ?? null;

  const submit = useCallback(() => {
    const client = getVenueClient();
    if (!inst) return;
    const lots = parsedQty;
    if (lots == null) {
      toast.error("Invalid quantity", { description: "Size must be a positive number of lots." });
      return;
    }
    if (lots > inst.max_order_lots) {
      toast.error("Above max order size", { description: `${inst.symbol} caps at ${inst.max_order_lots.toLocaleString()} lots.` });
      return;
    }
    let orderType;
    switch (kind) {
      case "limit":
        if (parsedPriceTicks == null) {
          toast.error("Limit price required", { description: "Enter a price in ticks or click a book level." });
          return;
        }
        orderType = { kind: "limit" as const };
        break;
      case "market":
        orderType = { kind: "market" as const };
        break;
      case "stop_market": {
        const t = Number(trigger);
        if (!Number.isFinite(t) || t <= 0) {
          toast.error("Trigger price required", { description: "Stop orders need a trigger (ticks)." });
          return;
        }
        orderType = { kind: "stop_market" as const, trigger_price: Math.floor(t) };
        break;
      }
      case "stop_limit": {
        const t = Number(trigger);
        if (parsedPriceTicks == null || !Number.isFinite(t) || t <= 0) {
          toast.error("Stop-limit needs trigger + limit", { description: "Both trigger and limit prices (ticks) are required." });
          return;
        }
        orderType = { kind: "stop_limit" as const, trigger_price: Math.floor(t), limit_price: parsedPriceTicks };
        break;
      }
      case "trailing_market":
      case "trailing_limit": {
        const o = Number(offset);
        if (!Number.isFinite(o) || o <= 0) {
          toast.error("Trail offset required", { description: "Trailing stops need an offset in ticks." });
          return;
        }
        orderType =
          kind === "trailing_market"
            ? { kind: "trailing_stop_market" as const, offset_ticks: Math.floor(o) }
            : { kind: "trailing_stop_limit" as const, offset_ticks: Math.floor(o), limit_ticks: parsedPriceTicks ?? Math.floor(o) };
        break;
      }
    }
    const tifv =
      tif === "gtd"
        ? { kind: "gtd" as const, until: snapshot!.meta.now + Number(gtdMin) * 60_000 }
        : { kind: tif as "gtc" | "ioc" | "fok" };

    const request = limitOrder(account, activeSymbol, side, parsedPriceTicks ?? 0, lots, {
      order_type: orderType,
      tif: tifv,
      post_only: postOnly || (kind === "limit" && tif === "gtc" ? postOnly : false),
      reduce_only: reduceOnly,
      display_lots: iceberg ? Math.max(1, Number(icebergLots) || 1) : null,
      client_ts: Date.now(),
    });

    setSubmitting(true);
    if (oco) {
      const secondPrice = Number(ocoPrice);
      if (!Number.isFinite(secondPrice) || secondPrice <= 0) {
        toast.error("OCO needs both legs", { description: "Enter the bracket's second-leg price (ticks)." });
        setSubmitting(false);
        return;
      }
      const second = limitOrder(account, activeSymbol, side === "bid" ? "ask" : "bid", Math.floor(secondPrice), lots, {
        order_type: { kind: "limit" },
        tif: { kind: "gtc" },
        reduce_only: true,
        client_ts: Date.now(),
      });
      client.send({ type: "command", command: { type: "place_oco", first: request, second, now: snapshot!.meta.now } });
      toast.success("OCO bracket placed", {
        description: `${side.toUpperCase()} ${lots} @ ${parsedPriceTicks ?? "mkt"} · OCO ${Math.floor(secondPrice)}`,
      });
    } else {
      client.send({ type: "command", command: { type: "place", request, now: snapshot!.meta.now } });
      toast.success("Order submitted", {
        description: `Command::Place → ${side.toUpperCase()} ${lots} lots ${activeSymbol}`,
      });
    }
    setSubmitting(false);
  }, [inst, parsedQty, parsedPriceTicks, kind, trigger, offset, tif, gtdMin, postOnly, reduceOnly, iceberg, icebergLots, oco, ocoPrice, side, account, activeSymbol, snapshot]);

  const isMarketFamily = kind === "market" || kind === "stop_market" || kind === "trailing_market";
  const buyActive = side === "bid";

  return (
    <TooltipProvider delayDuration={200}>
      <div className="h-full flex flex-col min-h-0 overflow-y-auto scroll-thin px-3 py-3 gap-3">
        {/* Side */}
        <div className="grid grid-cols-2 gap-1.5" role="tablist" aria-label="Order side">
          <button
            role="tab"
            aria-selected={buyActive}
            onClick={() => setSide("bid")}
            className={cn(
              "h-9 rounded-lg font-semibold text-[13px] transition-all focus-glow",
              buyActive
                ? "bg-up/15 text-up border border-up/40 shadow-[0_0_16px_-6px] shadow-up/40"
                : "bg-muted/40 text-muted-foreground hover:text-foreground border border-transparent",
            )}
          >
            Buy / Long
          </button>
          <button
            role="tab"
            aria-selected={!buyActive}
            onClick={() => setSide("ask")}
            className={cn(
              "h-9 rounded-lg font-semibold text-[13px] transition-all focus-glow",
              !buyActive
                ? "bg-down/15 text-down border border-down/40 shadow-[0_0_16px_-6px] shadow-down/40"
                : "bg-muted/40 text-muted-foreground hover:text-foreground border border-transparent",
            )}
          >
            Sell / Short
          </button>
        </div>

        {/* Order type */}
        <div className="grid grid-cols-2 gap-2">
          <Field label="Type">
            <Select value={kind} onValueChange={(v) => setKind(v as OrderKind)}>
              <SelectTrigger size="sm" className="h-8 text-[12px] bg-transparent border-hairline">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="limit" className="text-xs">Limit</SelectItem>
                <SelectItem value="market" className="text-xs">Market</SelectItem>
                <SelectItem value="stop_market" className="text-xs">Stop · Market</SelectItem>
                <SelectItem value="stop_limit" className="text-xs">Stop · Limit</SelectItem>
                <SelectItem value="trailing_market" className="text-xs">Trail · Market</SelectItem>
                <SelectItem value="trailing_limit" className="text-xs">Trail · Limit</SelectItem>
              </SelectContent>
            </Select>
          </Field>
          <Field label="TIF">
            <Select value={tif} onValueChange={(v) => setTif(v as TifKind)} disabled={kind === "market"}>
              <SelectTrigger size="sm" className="h-8 text-[12px] bg-transparent border-hairline">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="gtc" className="text-xs">GTC</SelectItem>
                <SelectItem value="ioc" className="text-xs">IOC</SelectItem>
                <SelectItem value="fok" className="text-xs">FOK</SelectItem>
                <SelectItem value="gtd" className="text-xs">GTD</SelectItem>
              </SelectContent>
            </Select>
          </Field>
        </div>

        {tif === "gtd" && kind !== "market" && (
          <Field label="GTD validity (minutes)">
            <Input
              value={gtdMin}
              onChange={(e) => setGtdMin(e.target.value)}
              inputMode="numeric"
              className="h-8 num text-[12px]"
              placeholder="60"
            />
          </Field>
        )}

        {/* Price */}
        {!isMarketFamily && (
          <Field
            label={
              <span className="flex items-center gap-1">
                Limit price (ticks)
                {markTicks != null && (
                  <button
                    onClick={() => setPrice(String(markTicks))}
                    className="text-[9.5px] text-primary hover:underline font-medium"
                  >
                    mark {markTicks.toLocaleString()}
                  </button>
                )}
              </span>
            }
          >
            <Input
              value={price}
              onChange={(e) => setPrice(e.target.value)}
              inputMode="numeric"
              className="h-8 num text-[12px]"
              placeholder="79950"
            />
          </Field>
        )}

        {(kind === "stop_market" || kind === "stop_limit") && (
          <Field label="Trigger price (ticks)">
            <Input
              value={trigger}
              onChange={(e) => setTrigger(e.target.value)}
              inputMode="numeric"
              className="h-8 num text-[12px]"
              placeholder="79000"
            />
          </Field>
        )}

        {(kind === "trailing_market" || kind === "trailing_limit") && (
          <Field label="Trail offset (ticks)">
            <Input
              value={offset}
              onChange={(e) => setOffset(e.target.value)}
              inputMode="numeric"
              className="h-8 num text-[12px]"
              placeholder="500"
            />
          </Field>
        )}

        {/* Size */}
        <Field
          label={
            <span className="flex items-center gap-1">
              Size (lots)
              {inst && <span className="text-[9px] text-muted-foreground/60">1 lot = {sizeBase(1, inst)} {inst.base_symbol}</span>}
            </span>
          }
        >
          <div className="flex gap-1">
            {[25, 50, 75].map((p) => (
              <button
                key={p}
                onClick={() => {
                  const base = position ? Math.abs(position.signed_lots) : 100;
                  setQty(String(Math.max(1, Math.floor((base * p) / 100))));
                }}
                className="flex-1 h-8 rounded-md bg-muted/40 hover:bg-muted text-[10px] text-muted-foreground hover:text-foreground transition-colors focus-glow"
              >
                {p === 75 ? "¾" : p === 50 ? "½" : "¼"}{position ? "" : "%"}
              </button>
            ))}
            <Input
              value={qty}
              onChange={(e) => setQty(e.target.value)}
              inputMode="numeric"
              className="h-8 num text-[12px] flex-[2]"
              placeholder="10"
            />
          </div>
        </Field>

        {/* Toggles */}
        <div className="space-y-2">
          <Toggle
            checked={postOnly}
            onChange={setPostOnly}
            label="Post-only"
            hint="Reject if the order would cross the book (maker guaranteed)"
            icon={<Layers className="w-3 h-3" />}
          />
          <Toggle
            checked={reduceOnly}
            onChange={setReduceOnly}
            label="Reduce-only"
            hint="The order may only shrink the position"
            icon={<Zap className="w-3 h-3" />}
          />
          <Toggle
            checked={iceberg}
            onChange={setIceberg}
            label="Iceberg"
            hint="Rest with a visible display size; matching sees the full quantity"
            icon={<Info className="w-3 h-3" />}
          />
          {iceberg && (
            <div className="pl-6">
              <Input
                value={icebergLots}
                onChange={(e) => setIcebergLots(e.target.value)}
                inputMode="numeric"
                className="h-7 num text-[11px]"
                placeholder="display lots"
              />
            </div>
          )}
          {kind === "limit" && (
            <>
              <Toggle
                checked={oco}
                onChange={setOco}
                label="OCO bracket"
                hint="Place a second, opposite reduce-only leg; filling one pulls the other"
                icon={<Info className="w-3 h-3" />}
              />
              {oco && (
                <div className="pl-6">
                  <Input
                    value={ocoPrice}
                    onChange={(e) => setOcoPrice(e.target.value)}
                    inputMode="numeric"
                    className="h-7 num text-[11px]"
                    placeholder="second leg price (ticks)"
                  />
                </div>
              )}
            </>
          )}
        </div>

        <div className="flex-1" />

        {/* Estimates */}
        <div className="rounded-lg bg-muted/30 border border-hairline p-2.5 space-y-1.5 text-[10.5px]">
          <EstRow label="Est. margin" value={estMargin ? usd(estMargin) : "—"} />
          <EstRow
            label="Notional"
            value={
              inst && parsedQty && (parsedPriceTicks ?? markTicks) != null
                ? usd(BigInt(parsedQty) * inst.lot_size_base_minor * (inst.tick_size_quote_minor * BigInt(parsedPriceTicks ?? markTicks ?? 1)) / 10n ** BigInt(inst.base_decimals))
                : "—"
            }
          />
          <EstRow label="Taker fee cap" value={inst?.kind === "option" ? "12.5% of premium" : "4bp → 0.9bp by tier"} />
        </div>

        {/* Submit */}
        <Button
          disabled={submitting}
          onClick={submit}
          className={cn(
            "h-10 w-full font-semibold text-[13.5px] transition-all",
            buyActive
              ? "bg-up/90 hover:bg-up text-[#052018]"
              : "bg-down/90 hover:bg-down text-[#2a0709]",
          )}
        >
          {buyActive ? "Buy" : "Sell"} {parsedQty ?? "—"} {activeSymbol.split("-")[0]}
          {oco && " · OCO"}
        </Button>
        <p className="text-[9px] text-muted-foreground/50 text-center font-mono">
          Command::Place → OrderRequest → risk gates → match
        </p>
      </div>
    </TooltipProvider>
  );
});

function Field({ label, children }: { label: React.ReactNode; children: React.ReactNode }) {
  return (
    <div className="space-y-1">
      <Label className="text-[10.5px] uppercase tracking-wider text-muted-foreground flex items-center gap-1.5 min-h-[14px]">
        {label}
      </Label>
      {children}
    </div>
  );
}

function Toggle({
  checked,
  onChange,
  label,
  hint,
  icon,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  label: string;
  hint: string;
  icon: React.ReactNode;
}) {
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <label className="flex items-center justify-between gap-2 py-0.5 cursor-pointer group">
          <span className="flex items-center gap-1.5 text-[11.5px] text-muted-foreground group-hover:text-foreground transition-colors">
            {icon}
            {label}
          </span>
          <Switch checked={checked} onCheckedChange={onChange} className="scale-[0.85] data-[state=checked]:bg-primary/70" />
        </label>
      </TooltipTrigger>
      <TooltipContent side="left" className="max-w-[220px]">
        <p className="text-[10px] leading-relaxed">{hint}</p>
      </TooltipContent>
    </Tooltip>
  );
}

function EstRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-center justify-between">
      <span className="text-muted-foreground">{label}</span>
      <span className="num">{value}</span>
    </div>
  );
}
