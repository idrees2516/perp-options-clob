"use client";

/**
 * Portfolio — the account deep-dive.
 * Descriptor header + margin stat tiles, positions (click → terminal),
 * greeks, multi-collateral with an inline USD↔BTC converter, the fee-tier
 * ladder, and instant internal transfers. One subaccount at a time.
 */

import { memo, useMemo, useState } from "react";
import { useVenueStore, liveAccount } from "@/lib/venue-store";
import { getVenueClient } from "@perp/api-client";
import { FEE_TIERS, formatMoney, tryParseMoney } from "@perp/types";
import type { Health, Instrument } from "@perp/types";
import { usd, usdCompact, sizeBase, pnl, shortSymbol, healthLabel } from "@/lib/fmt";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
} from "@/components/ui/select";
import { ArrowLeftRight, ArrowRight, ChevronDown, Send } from "lucide-react";
import { toast } from "sonner";
import { cn } from "@/lib/utils";

export const PortfolioView = memo(function PortfolioView() {
  const snapshot = useVenueStore((s) => s.snapshot);
  const deltas = useVenueStore((s) => s.accountDeltas);
  const account = useVenueStore((s) => s.activeAccount);
  const setActiveAccount = useVenueStore((s) => s.setActiveAccount);
  const setActiveSymbol = useVenueStore((s) => s.setActiveSymbol);
  const setView = useVenueStore((s) => s.setView);

  const live = liveAccount(snapshot, deltas, account);
  const descriptor = snapshot?.accountDescriptors.find((d) => d.id === account) ?? null;
  const positions = snapshot?.positions[account] ?? [];
  const greeks = snapshot?.greeks[account] ?? [];
  const collateral = snapshot?.collateral[account] ?? [];

  const insts = useMemo(() => {
    const m = new Map<string, Instrument>();
    for (const i of snapshot?.instruments ?? []) m.set(i.symbol, i);
    return m;
  }, [snapshot]);

  const notional = useMemo(
    () => positions.reduce((s, p) => s + BigInt.asUintN(64, p.notional_quote_minor), 0n),
    [positions],
  );

  /** 24h notional traded by this subaccount (fills journal) — tiers keyed off it. */
  const vol24 = useMemo(() => {
    if (!snapshot) return 0n;
    const cutoff = snapshot.meta.now - 24 * 3600 * 1000;
    let sum = 0n;
    for (const f of snapshot.fills) {
      if (f.subaccount === account && f.ts >= cutoff) sum += BigInt.asUintN(64, f.notional_quote_minor);
    }
    return sum;
  }, [snapshot, account]);

  /** Largest threshold ≤ 24h volume. */
  const tierIndex = useMemo(() => {
    let idx = 0;
    FEE_TIERS.forEach((t, i) => {
      if (vol24 >= t.volume_threshold_quote_minor) idx = i;
    });
    return idx;
  }, [vol24]);

  /* ── convert form state ── */
  const [convertFrom, setConvertFrom] = useState<"USD" | "BTC">("USD");
  const [convertAmount, setConvertAmount] = useState("");
  const convertTo = convertFrom === "USD" ? "BTC" : "USD";
  const btcBalance = collateral.find((c) => c.currency === "BTC")?.balance_minor ?? 0n;

  function submitConvert(): void {
    if (!snapshot) return;
    const parsed = tryParseMoney(convertAmount, convertFrom === "USD" ? 2 : 8);
    if (parsed == null || parsed <= 0n) {
      toast.error("Invalid conversion amount", { description: "Enter a positive decimal amount." });
      return;
    }
    getVenueClient().send({
      type: "command",
      command: {
        type: "convert_collateral",
        subaccount: account,
        from: convertFrom,
        to: convertTo,
        from_amount_minor: parsed,
        now: snapshot.meta.now,
      },
    });
    toast.success(
      `Converting ${convertFrom === "USD" ? `$${formatMoney(parsed, 2)}` : `${formatMoney(parsed, 8)} BTC`} → ${convertTo}`,
      { description: "Command::ConvertCollateral → Event::CollateralConversion · zero fee at the oracle spot (G-17)" },
    );
    setConvertAmount("");
  }

  /* ── transfer form state ── */
  const [transferTo, setTransferTo] = useState<number | null>(null);
  const [transferAmount, setTransferAmount] = useState("");
  const others = useMemo(
    () => (snapshot?.accountDescriptors ?? []).filter((d) => d.id !== account),
    [snapshot, account],
  );
  const toId =
    transferTo != null && others.some((d) => d.id === transferTo) ? transferTo : (others[0]?.id ?? null);
  const toDescriptor = others.find((d) => d.id === toId) ?? null;

  function submitTransfer(): void {
    if (!snapshot || toId == null) return;
    const parsed = tryParseMoney(transferAmount, 2);
    if (parsed == null || parsed <= 0n) {
      toast.error("Invalid transfer amount", { description: "Enter a positive USD amount." });
      return;
    }
    const toName = snapshot.accountDescriptors.find((d) => d.id === toId)?.name ?? `Sub ${toId}`;
    getVenueClient().send({
      type: "command",
      command: { type: "transfer", from: account, to: toId, amount_quote_minor: parsed, now: snapshot.meta.now },
    });
    toast.success(`Transfer ${usd(parsed)} → ${toName} submitted`, {
      description: "Command::Transfer → Event::TransferExecuted",
    });
    setTransferAmount("");
  }

  if (!live) {
    return (
      <div className="h-full flex items-center justify-center">
        <p className="text-[11px] text-muted-foreground/50">No account snapshot yet — awaiting bootstrap…</p>
      </div>
    );
  }

  return (
    <div className="h-full flex flex-col min-h-0">
      {/* Toolbar */}
      <div className="h-11 shrink-0 flex items-center gap-2.5 px-3 border-b border-hairline">
        <Select value={String(account)} onValueChange={(v) => setActiveAccount(Number(v))}>
          <SelectTrigger
            size="sm"
            aria-label="Active subaccount"
            className="h-7 w-[170px] gap-1.5 px-2 text-[11.5px] font-medium border-hairline bg-transparent"
          >
            <span
              className="w-2 h-2 rounded-full shrink-0"
              style={{ background: descriptor?.color ?? "#888" }}
            />
            {descriptor?.name ?? `Sub ${account}`}
            <ChevronDown className="w-3 h-3 opacity-40 ml-auto" aria-hidden />
          </SelectTrigger>
          <SelectContent>
            {(snapshot?.accountDescriptors ?? []).map((d) => (
              <SelectItem key={d.id} value={String(d.id)} className="text-xs py-1.5">
                <span className="flex items-center gap-2 w-56">
                  <span className="w-2 h-2 rounded-full shrink-0" style={{ background: d.color }} />
                  <span className="font-medium">{d.name}</span>
                  <span className="text-[10px] text-muted-foreground truncate">{d.role}</span>
                </span>
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <div className="flex-1" />
        <p className="text-[10px] text-muted-foreground/60 font-mono">
          {positions.length} positions · {usdCompact(notional)} notional · {greeks.length} greeks rows
        </p>
      </div>

      <div className="flex-1 min-h-0 overflow-auto scroll-thin p-3">
        {/* Account header: descriptor chip + health + margin tiles */}
        <section aria-label="Account overview" className="flex flex-wrap gap-2 mb-3">
          <div className="panel p-3 flex items-center gap-3 min-w-[230px]">
            <span
              className="w-2.5 h-2.5 rounded-full shrink-0"
              style={{ background: descriptor?.color ?? "#888" }}
            />
            <div className="leading-tight">
              <p className="text-[13px] font-semibold">{descriptor?.name ?? `Sub ${account}`}</p>
              <p className="text-[9.5px] text-muted-foreground">{descriptor?.role ?? "—"}</p>
            </div>
            <HealthBadge h={live.health} />
          </div>
          <div className="grid grid-cols-2 sm:grid-cols-3 xl:grid-cols-5 gap-2 flex-1 min-w-[300px]">
            <StatTile label="Equity" value={usd(live.equity_quote_minor)} strong />
            <StatTile label="Cash" value={usd(live.cash_quote_minor)} />
            <StatTile label="Order margin" value={usd(BigInt.asUintN(64, live.order_margin_quote_minor))} />
            <StatTile label="Initial margin" value={usd(live.initial_quote_minor)} muted />
            <StatTile label="Maintenance" value={usd(live.maintenance_quote_minor)} muted />
          </div>
        </section>

        {/* Positions */}
        <section aria-label="Positions" className="panel mb-3 overflow-hidden">
          <PanelHeader
            title="Positions"
            hint="click a row to load the instrument in the terminal"
            right={
              <span className="text-[9px] font-mono text-muted-foreground/60">{positions.length} open</span>
            }
          />
          <div className="max-h-88 overflow-auto scroll-thin">
            <table className="w-full text-[11.5px] border-collapse">
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
                    <tr
                      key={p.symbol}
                      onClick={() => {
                        setActiveSymbol(p.symbol);
                        setView("terminal");
                      }}
                      className="hover:bg-primary/5 cursor-pointer border-b border-hairline/40"
                    >
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
            </table>
          </div>
        </section>

        {/* Greeks + collateral */}
        <div className="grid grid-cols-1 xl:grid-cols-12 gap-3 mb-3">
          <section aria-label="Greeks" className="panel xl:col-span-5 overflow-hidden flex flex-col">
            <PanelHeader title="Greeks" hint="SFPM scenario grid" />
            <div className="max-h-72 overflow-auto scroll-thin">
              <table className="w-full text-[11.5px] border-collapse">
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
                    <tr key={g.symbol} className="hover:bg-muted/30 border-b border-hairline/40">
                      <Td className="font-mono text-[11px]">{shortSymbol(g.symbol)}</Td>
                      <Td right>{g.delta.toFixed(3)}</Td>
                      <Td right className="text-muted-foreground">{g.gamma.toFixed(3)}</Td>
                      <Td right className="text-muted-foreground">{g.vega.toFixed(3)}</Td>
                      <Td right className="text-muted-foreground">{g.theta.toFixed(3)}</Td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </section>

          <section aria-label="Collateral" className="panel xl:col-span-7 overflow-hidden flex flex-col">
            <PanelHeader
              title="Collateral"
              hint="G-17 multi-currency · haircut value in USD"
              right={
                <span className="text-[9px] font-mono text-muted-foreground/60">{collateral.length} currencies</span>
              }
            />
            <div className="max-h-72 overflow-auto scroll-thin">
              <table className="w-full text-[11.5px] border-collapse">
                <thead>
                  <tr>
                    <Th>Currency</Th>
                    <Th right>Balance</Th>
                    <Th right>Haircut value</Th>
                  </tr>
                </thead>
                <tbody>
                  {collateral.length === 0 && (
                    <EmptyRow cols={3} text="No non-quote collateral — convert USD → BTC below." />
                  )}
                  {collateral.map((c) => (
                    <tr key={c.currency} className="hover:bg-muted/30 border-b border-hairline/40">
                      <Td className="font-mono text-[11px]">{c.currency}</Td>
                      <Td right>{formatMoney(c.balance_minor, c.currency === "BTC" ? 8 : 2)}</Td>
                      <Td right className="text-muted-foreground">{usd(c.value_quote_minor)}</Td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
            <div className="mt-auto px-3 py-2.5 border-t border-hairline bg-panel-2/40 flex flex-wrap items-center gap-2">
              <span className="text-[9px] uppercase tracking-wider text-muted-foreground">Convert</span>
              <Select value={convertFrom} onValueChange={(v) => setConvertFrom(v === "BTC" ? "BTC" : "USD")}>
                <SelectTrigger
                  size="sm"
                  aria-label="Convert from currency"
                  className="h-7 w-[76px] text-[11px] num border-hairline bg-transparent"
                >
                  {convertFrom}
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="USD" className="text-xs">USD</SelectItem>
                  <SelectItem value="BTC" className="text-xs">BTC</SelectItem>
                </SelectContent>
              </Select>
              <ArrowRight className="w-3 h-3 text-muted-foreground/60" aria-hidden />
              <Select value={convertTo} onValueChange={(v) => setConvertFrom(v === "BTC" ? "USD" : "BTC")}>
                <SelectTrigger
                  size="sm"
                  aria-label="Convert to currency"
                  className="h-7 w-[76px] text-[11px] num border-hairline bg-transparent"
                >
                  {convertTo}
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="USD" className="text-xs">USD</SelectItem>
                  <SelectItem value="BTC" className="text-xs">BTC</SelectItem>
                </SelectContent>
              </Select>
              <Input
                value={convertAmount}
                onChange={(e) => setConvertAmount(e.target.value)}
                placeholder={convertFrom === "USD" ? "USD amount" : "BTC amount"}
                inputMode="decimal"
                aria-label="Conversion amount"
                className="h-7 w-28 text-[11px] num bg-transparent border-hairline"
              />
              <span className="text-[9.5px] text-muted-foreground/70 num">
                {convertFrom === "USD"
                  ? `avail ${usd(live.cash_quote_minor)}`
                  : `avail ${formatMoney(btcBalance, 8)} BTC`}
              </span>
              <div className="flex-1" />
              <Button size="sm" className="h-7 text-[11px] gap-1.5" onClick={submitConvert}>
                <ArrowLeftRight className="w-3 h-3" />
                Convert
              </Button>
            </div>
          </section>
        </div>

        {/* Fee ladder + transfers */}
        <div className="grid grid-cols-1 xl:grid-cols-12 gap-3">
          <section aria-label="Fees and tiers" className="panel xl:col-span-7 overflow-hidden flex flex-col">
            <PanelHeader
              title="Fees & tiers"
              hint="30-day trailing notional ladder"
              right={
                <span className="text-[9.5px] font-mono text-muted-foreground">
                  24h volume {usdCompact(vol24)}
                </span>
              }
            />
            <table className="w-full text-[11.5px] border-collapse">
              <thead>
                <tr>
                  <Th>Tier</Th>
                  <Th right>30-day volume</Th>
                  <Th right>Maker</Th>
                  <Th right>Taker</Th>
                  <Th />
                </tr>
              </thead>
              <tbody>
                {FEE_TIERS.map((t, i) => (
                  <tr
                    key={t.name}
                    className={cn(
                      "border-b border-hairline/40",
                      i === tierIndex ? "bg-primary/[0.07]" : "hover:bg-muted/25",
                    )}
                  >
                    <Td className="font-medium">
                      {t.name}
                      {i === tierIndex && (
                        <span className="ml-2 text-[8px] font-mono px-1 py-px rounded border border-primary/30 bg-primary/10 text-primary">
                          YOU ARE HERE
                        </span>
                      )}
                    </Td>
                    <Td right className="text-muted-foreground">
                      {i === 0 ? "—" : `≥ ${usdCompact(t.volume_threshold_quote_minor)}`}
                    </Td>
                    <Td right className={t.maker_bps < 0 ? "text-up" : undefined}>
                      {feeBps(t.maker_bps)}
                      {t.maker_bps < 0 && <span className="ml-1 text-[8px] uppercase tracking-wide">rebate</span>}
                    </Td>
                    <Td right>{feeBps(t.taker_bps)}</Td>
                    <Td />
                  </tr>
                ))}
              </tbody>
            </table>
            <div className="mt-auto px-3 py-2 border-t border-hairline bg-panel-2/40 flex flex-wrap items-center gap-x-5 gap-y-1">
              <MiniStat label="Fees paid" value={usd(live.fees_paid_quote_minor)} />
              <MiniStat
                label="Funding received"
                value={usd(live.funding_received_quote_minor, { sign: true })}
                className={cn(
                  "num text-[11.5px]",
                  live.funding_received_quote_minor > 0n
                    ? "text-up"
                    : live.funding_received_quote_minor < 0n
                      ? "text-down"
                      : "text-muted-foreground",
                )}
              />
              <div className="flex-1" />
              <span className="text-[9px] text-muted-foreground/50 font-mono">
                option premium fee caps (F-2): 12.5% taker · 2.5% maker
              </span>
            </div>
          </section>

          <section aria-label="Transfers" className="panel xl:col-span-5 overflow-hidden flex flex-col">
            <PanelHeader title="Transfers" hint="internal · instant · no fee" />
            <div className="p-3 flex flex-col gap-2.5">
              <div className="flex items-start gap-2">
                <div className="flex-1 min-w-0">
                  <p className="text-[9px] uppercase tracking-wider text-muted-foreground mb-1">From</p>
                  <div className="h-8 px-2.5 rounded-md border border-hairline bg-panel-2/50 flex items-center gap-2">
                    <span
                      className="w-2 h-2 rounded-full shrink-0"
                      style={{ background: descriptor?.color ?? "#888" }}
                    />
                    <span className="text-[11.5px] font-medium truncate">
                      {descriptor?.name ?? `Sub ${account}`}
                    </span>
                    <span className="num text-[10px] text-muted-foreground ml-auto">{usd(live.cash_quote_minor)}</span>
                  </div>
                </div>
                <ArrowLeftRight className="w-3.5 h-3.5 text-muted-foreground/50 mt-[26px] shrink-0" aria-hidden />
                <div className="flex-1 min-w-0">
                  <p className="text-[9px] uppercase tracking-wider text-muted-foreground mb-1">To</p>
                  <Select value={toId == null ? undefined : String(toId)} onValueChange={(v) => setTransferTo(Number(v))}>
                    <SelectTrigger
                      size="sm"
                      aria-label="Transfer destination subaccount"
                      className="w-full h-8 text-[11.5px] border-hairline bg-panel-2/50"
                    >
                      {toDescriptor ? (
                        <span className="flex items-center gap-2 min-w-0">
                          <span
                            className="w-2 h-2 rounded-full shrink-0"
                            style={{ background: toDescriptor.color }}
                          />
                          <span className="font-medium truncate">{toDescriptor.name}</span>
                        </span>
                      ) : (
                        <span className="text-muted-foreground">No other subaccount</span>
                      )}
                    </SelectTrigger>
                    <SelectContent>
                      {others.map((d) => (
                        <SelectItem key={d.id} value={String(d.id)} className="text-xs py-1.5">
                          <span className="flex items-center gap-2">
                            <span className="w-2 h-2 rounded-full shrink-0" style={{ background: d.color }} />
                            <span className="font-medium">{d.name}</span>
                            <span className="text-[10px] text-muted-foreground truncate">{d.role}</span>
                          </span>
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                </div>
              </div>
              <div className="flex items-end gap-2">
                <div className="flex-1">
                  <p className="text-[9px] uppercase tracking-wider text-muted-foreground mb-1">Amount (USD)</p>
                  <Input
                    value={transferAmount}
                    onChange={(e) => setTransferAmount(e.target.value)}
                    placeholder="e.g. 25000.00"
                    inputMode="decimal"
                    aria-label="Transfer amount in USD"
                    className="h-8 text-[11.5px] num bg-transparent border-hairline"
                  />
                </div>
                <Button
                  size="sm"
                  className="h-8 text-[11.5px] gap-1.5"
                  onClick={submitTransfer}
                  disabled={toId == null}
                >
                  <Send className="w-3 h-3" />
                  Transfer
                </Button>
              </div>
              <p className="text-[9px] text-muted-foreground/50 font-mono">
                Command::Transfer → Event::TransferExecuted · margin pooled per subaccount
              </p>
            </div>
          </section>
        </div>
      </div>
    </div>
  );
});

/* ─────────────────────────── local bits ─────────────────────────── */

function feeBps(x: number): string {
  return `${x < 0 ? "−" : ""}${Math.abs(x).toFixed(1)}bp`;
}

function HealthBadge({ h }: { h: Health }) {
  const cls =
    h === "healthy"
      ? "border-up/30 bg-up/10 text-up"
      : h === "restricted"
        ? "border-amber-400/30 bg-amber-400/10 text-amber-400"
        : "border-down/30 bg-down/10 text-down";
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1.5 px-2 py-1 rounded-md border text-[10px] font-medium whitespace-nowrap",
        cls,
      )}
    >
      <span className="w-1.5 h-1.5 rounded-full bg-current" aria-hidden />
      {healthLabel(h)}
    </span>
  );
}

function StatTile({
  label,
  value,
  strong,
  muted,
}: {
  label: string;
  value: string;
  strong?: boolean;
  muted?: boolean;
}) {
  return (
    <div className="panel p-3 flex flex-col justify-center">
      <p className="text-[9px] uppercase tracking-wider text-muted-foreground">{label}</p>
      <p
        className={cn(
          "num",
          strong ? "text-lg font-semibold" : "text-base font-medium",
          muted && "text-muted-foreground",
        )}
      >
        {value}
      </p>
    </div>
  );
}

function MiniStat({ label, value, className }: { label: string; value: string; className?: string }) {
  return (
    <div className="flex flex-col leading-none">
      <span className="text-[8.5px] uppercase tracking-wider text-muted-foreground/60 mb-0.5">{label}</span>
      <span className={cn("num text-[11.5px]", className)}>{value}</span>
    </div>
  );
}

function PanelHeader({
  title,
  hint,
  right,
}: {
  title: string;
  hint?: string;
  right?: React.ReactNode;
}) {
  return (
    <header className="px-3 py-2 border-b border-hairline flex items-center gap-2">
      <h2 className="text-[11px] font-semibold tracking-tight">{title}</h2>
      {hint && <span className="text-[9.5px] text-muted-foreground/70 hidden sm:inline">{hint}</span>}
      <div className="flex-1" />
      {right}
    </header>
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

function Td({ children, right, className }: { children?: React.ReactNode; right?: boolean; className?: string }) {
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
