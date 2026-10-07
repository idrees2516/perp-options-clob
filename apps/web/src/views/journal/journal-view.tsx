"use client";

/**
 * The Journal — the event stream itself, the protocol's soul.
 * Every state change is an Event; replay reconstructs state bit-for-bit.
 * Rows render newest-first with a one-line summarizer per event type;
 * a click expands the full journaled payload.
 */

import { memo, useMemo, useState } from "react";
import { useVenueStore } from "@/lib/venue-store";
import {
  bps,
  iv as fmtIv,
  pnl,
  priceQuote,
  relTime,
  shortSymbol,
  simTime,
  ticksToPrice,
  usd,
  usdCompact,
} from "@/lib/fmt";
import { EVENT_TYPES } from "@perp/types";
import type { Event, EventType, Instrument, JournalEntry, Rejection } from "@perp/types";
import { formatMoney } from "@perp/types";
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
import { Button } from "@/components/ui/button";
import { ChevronDown, Pause, Play, Search } from "lucide-react";

/* ─────────────────────────── categories ─────────────────────────── */

type Category = "fill" | "order" | "risk" | "funding" | "account" | "market";

const CATEGORY: Record<EventType, Category> = {
  trade_executed: "fill",
  rfq_settled: "fill",
  block_registered: "fill",
  block_printed: "fill",

  order_resting: "order",
  order_closed: "order",
  order_rejection: "order",
  mmp_configured: "order",
  order_amended: "order",
  trailing_updated: "order",
  stp_cancels: "order",
  oco_linked: "order",
  twap_opened: "order",
  twap_sliced: "order",
  twap_closed: "order",
  auction_opened: "order",
  auction_uncrossed: "order",
  rfq_created: "order",
  rfq_quoted: "order",
  rfq_closed: "order",

  liquidation: "risk",
  adl: "risk",
  breaker_tripped: "risk",
  market_halted: "risk",
  mmp_tripped: "risk",
  session_disconnected: "risk",
  withdraw_rejected: "risk",
  transfer_rejected: "risk",
  collateral_rejected: "risk",
  exercise_rejected: "risk",
  rfq_rejected: "risk",

  funding: "funding",
  funding_flow: "funding",
  option_expiry: "funding",
  exercise_queued: "funding",
  option_exercised: "funding",
  exercise_deferred: "funding",
  option_delisted: "funding",
  position_migrated: "funding",

  deposit: "account",
  withdrawal: "account",
  transfer_executed: "account",
  reward: "account",
  rewards_settled: "account",
  collateral_moved: "account",
  collateral_conversion: "account",
  collateral_interest_accrued: "account",
  quote_interest_accrued: "account",
  vault_opened: "account",
  vault_queued: "account",
  vault_epoch_settled: "account",
  insurance_marked: "account",
  insurance_rebalanced: "account",
  mm_enrolled: "account",
  mm_tier_adjusted: "account",
  cod_changed: "account",

  provider_observed: "market",
  market_listed: "market",
  clock_advanced: "market",
  surface_observed: "market",
  surface_swept: "market",
  vol_index_published: "market",
  market_resumed: "market",
  breaker_released: "market",
  liquidity_scored: "market",
};

const CATEGORY_CLASS: Record<Category, string> = {
  fill: "text-up bg-up/10",
  order: "text-primary bg-primary/10",
  risk: "text-down bg-down/10",
  funding: "text-chart-3 bg-chart-3/10",
  account: "text-chart-4 bg-chart-4/10",
  market: "text-muted-foreground bg-muted/40",
};

const CATEGORY_LABEL: Record<Category, string> = {
  fill: "Fills",
  order: "Orders",
  risk: "Risk",
  funding: "Funding & settlement",
  account: "Accounts & vaults",
  market: "Market data",
};

const CATEGORY_ORDER: Category[] = ["fill", "order", "risk", "funding", "account", "market"];

const GROUPED_TYPES: { category: Category; types: EventType[] }[] = CATEGORY_ORDER.map((category) => ({
  category,
  types: EVENT_TYPES.filter((t) => CATEGORY[t] === category),
}));

/* ─────────────────────────── the view ─────────────────────────── */

const RENDER_CAP = 400;

export const JournalView = memo(function JournalView() {
  const snapshot = useVenueStore((s) => s.snapshot);
  const journal = useVenueStore((s) => s.journal);
  const [query, setQuery] = useState("");
  const [typeFilter, setTypeFilter] = useState<EventType | "all">("all");
  const [expanded, setExpanded] = useState<number | null>(null);

  /* Pause: freeze the rendered list by snapshotting it into state; entries
     that arrive while paused accumulate in the store journal (the buffer)
     and are counted per render from the seq watermark, then flushed by
     clearing the snapshot on resume. */
  const [frozen, setFrozen] = useState<{ entries: JournalEntry[]; lastSeq: number } | null>(null);
  const paused = frozen !== null;
  const display = frozen ? frozen.entries : journal;
  const held = frozen ? journal.filter((j) => j.seq > frozen.lastSeq).length : 0;

  const instMap = useMemo(() => {
    const m = new Map<string, Instrument>();
    for (const i of snapshot?.instruments ?? []) m.set(i.symbol, i);
    return m;
  }, [snapshot]);
  const instOf = useMemo(() => (symbol: string) => instMap.get(symbol), [instMap]);
  const now = snapshot?.meta.now ?? Date.now();

  const rows = useMemo(() => {
    const q = query.trim().toLowerCase();
    let list = display;
    if (typeFilter !== "all") list = list.filter((j) => j.event.type === typeFilter);
    if (q) list = list.filter((j) => searchable(j).includes(q));
    return list.slice(-RENDER_CAP).reverse();
  }, [display, query, typeFilter]);

  const togglePause = () => {
    if (!paused) {
      const lastSeq = journal.length > 0 ? journal[journal.length - 1]!.seq : 0;
      setFrozen({ entries: journal.slice(), lastSeq });
    } else {
      // Flush — the store journal already carries every buffered entry.
      setFrozen(null);
      setExpanded(null);
    }
  };

  return (
    <div className="h-full flex flex-col min-h-0">
      {/* toolbar */}
      <div className="h-11 shrink-0 flex items-center gap-2.5 px-3 border-b border-hairline">
        <div className="relative w-52">
          <Search className="absolute left-2.5 top-1/2 -translate-y-1/2 w-3.5 h-3.5 text-muted-foreground/60" />
          <Input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Filter — type, symbol, subaccount…"
            className="h-8 pl-8 text-[12px] bg-transparent border-hairline"
          />
        </div>
        <Select value={typeFilter} onValueChange={(v) => setTypeFilter(v as EventType | "all")}>
          <SelectTrigger size="sm" className="h-8 w-52 text-[11px] font-mono bg-transparent border-hairline">
            <SelectValue placeholder="event type" />
          </SelectTrigger>
          <SelectContent className="max-h-80">
            <SelectItem value="all" className="text-[11px] font-mono">
              all events
            </SelectItem>
            {GROUPED_TYPES.map(({ category, types }) => (
              <SelectGroup key={category}>
                <SelectLabel className="text-[9px] uppercase tracking-wider">{CATEGORY_LABEL[category]}</SelectLabel>
                {types.map((t) => (
                  <SelectItem key={t} value={t} className="text-[11px] font-mono">
                    {t}
                  </SelectItem>
                ))}
              </SelectGroup>
            ))}
          </SelectContent>
        </Select>
        <Button
          variant={paused ? "default" : "ghost"}
          size="sm"
          className={`h-8 text-[10px] font-mono ${paused ? "" : "text-muted-foreground border border-hairline"}`}
          onClick={togglePause}
          aria-label={paused ? "Resume the stream" : "Pause the stream"}
        >
          {paused ? <Play className="w-3 h-3" /> : <Pause className="w-3 h-3" />}
          {paused ? `resume (+${held} held)` : "pause"}
        </Button>
        <span className="flex items-center gap-1.5 text-[9.5px] font-mono text-muted-foreground/70">
          <span className={`w-1.5 h-1.5 rounded-full ${paused ? "bg-muted-foreground/50" : "bg-up pulse-dot"}`} />
          {paused ? "frozen" : "receiving"}
        </span>
        <div className="flex-1" />
        <p className="text-[10px] text-muted-foreground/60 font-mono hidden md:block">{rows.length} shown</p>
      </div>

      {/* stats strip */}
      <div className="h-7 shrink-0 flex items-center gap-3 px-3 border-b border-hairline bg-panel/60 text-[10px]">
        <Stat label="events" value={(snapshot?.stats.events ?? 0).toLocaleString()} />
        <Stat label="journal" value={journal.length.toLocaleString()} />
        <span className="text-muted-foreground/50 text-[9.5px] italic hidden sm:block">
          every state change is an Event — replay reconstructs state bit-for-bit
        </span>
      </div>

      {/* stream */}
      <div className="flex-1 min-h-0 overflow-auto scroll-thin" role="feed" aria-label="Event journal">
        {rows.length === 0 && (
          <div className="h-full flex items-center justify-center">
            <p className="text-muted-foreground/50 text-[11px]">No events match — the stream is quiet here.</p>
          </div>
        )}
        {rows.map((entry) => (
          <JournalRow
            key={entry.seq}
            entry={entry}
            instOf={instOf}
            now={now}
            expanded={expanded === entry.seq}
            onToggle={() => setExpanded(expanded === entry.seq ? null : entry.seq)}
          />
        ))}
      </div>
    </div>
  );
});

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <span className="flex items-baseline gap-1.5">
      <span className="uppercase tracking-wider text-muted-foreground/60 text-[9px]">{label}</span>
      <span className="num text-muted-foreground">{value}</span>
    </span>
  );
}

function JournalRow({
  entry,
  instOf,
  now,
  expanded,
  onToggle,
}: {
  entry: JournalEntry;
  instOf: (symbol: string) => Instrument | undefined;
  now: number;
  expanded: boolean;
  onToggle: () => void;
}) {
  const e = entry.event;
  const category = CATEGORY[e.type] ?? "market";
  const summary = useMemo(() => summarize(e, instOf, now), [e, instOf, now]);

  return (
    <div className="border-b border-hairline/40">
      <button
        onClick={onToggle}
        className="w-full text-left grid grid-cols-[56px_58px_auto_1fr_16px] items-center gap-2 px-3 h-7 hover:bg-muted/30 transition-colors"
        aria-expanded={expanded}
      >
        <span className="num text-[10px] text-muted-foreground/50 text-right">{entry.seq}</span>
        <span className="num text-[10px] text-muted-foreground/70">{simTime(entry.ts)}</span>
        <span
          className={`text-[9px] font-mono px-1.5 py-px rounded justify-self-start whitespace-nowrap ${CATEGORY_CLASS[category]}`}
        >
          {e.type}
        </span>
        <span className="text-[11px] text-muted-foreground truncate">{summary}</span>
        <ChevronDown
          className={`w-3 h-3 text-muted-foreground/40 transition-transform ${expanded ? "rotate-180" : ""}`}
        />
      </button>
      {expanded && (
        <pre className="mx-3 mb-2 p-2.5 rounded-md bg-panel-2 border border-hairline/60 text-[10px] font-mono leading-relaxed max-h-64 overflow-auto scroll-thin whitespace-pre">
          {JSON.stringify(
            e,
            (_k, v) => (typeof v === "bigint" ? `${v}n` : v),
            2,
          )}
        </pre>
      )}
    </div>
  );
}

/* ─────────────────────────── search text ─────────────────────────── */

const SYMBOL_KEYS = new Set(["symbol", "base_symbol", "from_symbol", "to_symbol"]);
const SUB_KEYS = new Set([
  "subaccount",
  "taker",
  "maker",
  "from",
  "to",
  "liquidated_subaccount",
  "counterparty_subaccount",
  "taker_subaccount",
  "maker_subaccount",
]);

function searchable(entry: JournalEntry): string {
  const parts: string[] = [entry.event.type];
  collectSearch(entry.event, parts, 0);
  return parts.join(" ").toLowerCase();
}

function collectSearch(v: unknown, parts: string[], depth: number): void {
  if (v == null || depth > 4 || typeof v !== "object") return;
  if (Array.isArray(v)) {
    for (const item of v) collectSearch(item, parts, depth + 1);
    return;
  }
  for (const [key, val] of Object.entries(v as Record<string, unknown>)) {
    if (SYMBOL_KEYS.has(key) || SUB_KEYS.has(key)) parts.push(String(val));
    else collectSearch(val, parts, depth + 1);
  }
}

/* ─────────────────────────── the summarizer ─────────────────────────── */

function rejectionToken(r: Rejection): string {
  switch (r.kind) {
    case "outside_price_band":
      return `outside_price_band (mark ${usd(r.mark_quote_minor)})`;
    case "position_limit_exceeded":
      return `position_limit_exceeded (${r.projected_lots}/${r.max_lots} lots)`;
    case "too_many_open_orders":
      return `too_many_open_orders (${r.current}/${r.max})`;
    case "insufficient_margin":
      return `insufficient_margin (shortfall ${usd(r.shortfall)})`;
    case "greeks_limit_exceeded":
      return `greeks_limit_exceeded (${r.what} ${r.would_be.toFixed(2)}/${r.cap})`;
    default:
      return r.kind;
  }
}

function summarize(e: Event, instOf: (symbol: string) => Instrument | undefined, now: number): string {
  const priceOf = (symbol: string, ticks: number): string => ticksToPrice(ticks, instOf(symbol) ?? undefined);
  const dec = (currency: string): number => (currency === "BTC" ? 8 : 2);

  switch (e.type) {
    case "deposit":
      return `${usd(e.amount_quote_minor, { sign: true })} to sub ${e.subaccount}`;
    case "withdrawal":
      return `${usd(0n - e.amount_quote_minor)} · sub ${e.subaccount} settled`;
    case "withdraw_rejected":
      return `${usd(e.requested)} rejected · ${e.reason}`;
    case "provider_observed":
      return `${e.provider} ${priceQuote(e.price_quote_minor)}`;
    case "market_listed":
      return `${shortSymbol(e.instrument.symbol)} listed · anchor IV ${e.anchor_iv_bps != null ? fmtIv(e.anchor_iv_bps / 10_000) : "—"}`;
    case "clock_advanced":
      return `engine clock ${simTime(e.now)}`;
    case "order_resting": {
      const o = e.order;
      return (
        `${shortSymbol(o.symbol)} ${o.side} ${o.qty_lots} lots` +
        (o.price_ticks != null ? ` @ ${priceOf(o.symbol, o.price_ticks)}` : " MKT") +
        ` resting · margin ${usdCompact(e.margin_reserved_quote_minor)}`
      );
    }
    case "order_closed":
      return `#${e.order_id} ${e.reason} · ${shortSymbol(e.symbol)}`;
    case "order_rejection":
      return `reason: ${rejectionToken(e.payload.reason)}`;
    case "trade_executed": {
      const t = e.payload;
      const fee =
        t.taker_fee_quote_minor >= 0n
          ? `fee ${usd(t.taker_fee_quote_minor)}`
          : `rebate ${usd(-t.taker_fee_quote_minor, { sign: true })}`;
      return `${priceOf(t.symbol, t.price_ticks)} × ${t.qty_lots} · ${usd(t.notional_quote_minor)} · ${fee}`;
    }
    case "stp_cancels":
      return `${e.maker_ids.length} resting orders canceled (STP)`;
    case "funding":
      return `${shortSymbol(e.payload.symbol)} rate ${bps(e.payload.rate_bps)} per interval`;
    case "funding_flow":
      return `${pnl(e.payload.credit_quote_minor)} to sub ${e.payload.subaccount}`;
    case "option_expiry":
      return `${shortSymbol(e.payload.symbol)} settled · ${e.payload.signed_lots} lots · ${pnl(e.payload.payout_quote_minor)}`;
    case "exercise_queued":
      return `${shortSymbol(e.symbol)} ${e.lots} lots · settles ${relTime(e.settle_at, now)}`;
    case "option_exercised":
      return `${e.payload.settled_lots}/${e.payload.requested_lots} lots · intrinsic ${usd(e.payload.intrinsic_per_lot_quote_minor)}/lot · ${pnl(e.payload.gross_payout_quote_minor)}`;
    case "exercise_rejected":
      return `${shortSymbol(e.symbol)} ${e.requested_lots} lots · ${e.reason}`;
    case "exercise_deferred":
      return `request #${e.request_id} deferred to ${simTime(e.new_settle_at)}`;
    case "option_delisted":
      return `${shortSymbol(e.symbol)} delisted`;
    case "liquidity_scored":
      return `${e.observations.length} maker observations scored`;
    case "reward":
      return `${usd(e.payload.amount_quote_minor, { sign: true })} to sub ${e.payload.subaccount}`;
    case "rewards_settled":
      return "reward epoch settled";
    case "liquidation":
      return `${e.payload.lots} lots @ ${usd(e.payload.price_quote_minor)} · ${e.payload.to_insurance ? "to insurance" : "to book"} · absorbed ${usdCompact(e.payload.absorbed_quote_minor)}`;
    case "adl":
      return `sub ${e.payload.liquidated_subaccount} ↔ sub ${e.payload.counterparty_subaccount} · ${e.payload.lots} lots @ ${usd(e.payload.price_quote_minor)}`;
    case "market_halted":
      return `${e.base_symbol} halted`;
    case "market_resumed":
      return `${e.base_symbol} resumed`;
    case "rfq_created":
      return `${e.legs.length} legs · taker sub ${e.taker} · ${e.counterparties.length} dealers`;
    case "rfq_quoted":
      return `#${e.rfq_id} · maker sub ${e.maker} · ${e.leg_prices_ticks.length} legs priced`;
    case "rfq_settled":
      return `${e.trades.length} legs executed · quote #${e.quote_id}`;
    case "rfq_rejected":
      return `sub ${e.subaccount} · ${e.reason}`;
    case "rfq_closed":
      return `#${e.rfq_id} ${e.reason}`;
    case "cod_changed":
      return `sub ${e.subaccount} ${e.enabled ? "enabled" : "disabled"} cancel-on-disconnect`;
    case "block_registered":
      return `${e.legs.length} legs · ${usdCompact(e.total_notional_quote_minor)} · sub ${e.taker} ↔ sub ${e.maker}`;
    case "block_printed":
      return `block #${e.block_id} printed to tape`;
    case "transfer_executed":
      return `${usd(e.amount_quote_minor)} · sub ${e.from} → sub ${e.to}`;
    case "transfer_rejected":
      return `${usd(e.requested)} rejected · ${e.reason}`;
    case "mmp_configured":
      return `sub ${e.subaccount} · ${e.amount_limit_lots} lots / ${e.interval_ms}ms · Δ ±${e.delta_limit_lots}`;
    case "mmp_tripped":
      return `sub ${e.subaccount} ${e.base_symbol} quotes frozen`;
    case "session_disconnected":
      return `sub ${e.subaccount} dropped · ${e.canceled_orders.length} orders canceled`;
    case "surface_observed":
      return `${shortSymbol(e.payload.symbol)} iv ${(e.payload.iv_bps / 100).toFixed(1)}%`;
    case "surface_swept":
      return `iv surface swept @ ${simTime(e.now)}`;
    case "breaker_tripped":
      return `${shortSymbol(e.symbol)} · ${e.kind}`;
    case "breaker_released":
      return `${shortSymbol(e.symbol)} · ${e.kind} released`;
    case "order_amended":
      return `#${e.payload.order_id} → ${e.payload.new_open_lots} open lots`;
    case "trailing_updated":
      return `#${e.order_id} extreme ${usd(e.extreme_quote_minor)}`;
    case "auction_opened":
      return `${shortSymbol(e.symbol)} · uncross ${simTime(e.uncross_at)}`;
    case "auction_uncrossed":
      return `${shortSymbol(e.symbol)} · ${e.matched_lots} lots matched${e.clearing_price_ticks != null ? ` @ ${priceOf(e.symbol, e.clearing_price_ticks)}` : " · no clearing"}`;
    case "collateral_moved":
      return `${e.payload.currency} ${formatMoney(e.payload.amount_minor, dec(e.payload.currency))} · sub ${e.payload.subaccount}`;
    case "collateral_rejected":
      return `${e.currency} ${formatMoney(e.requested_minor, dec(e.currency))} · ${e.reason}`;
    case "collateral_conversion":
      return `${formatMoney(e.payload.from_amount_minor, dec(e.payload.from))} ${e.payload.from} → ${e.payload.to}`;
    case "position_migrated":
      return `${e.signed_lots} lots ${shortSymbol(e.from_symbol)} → ${shortSymbol(e.to_symbol)}`;
    case "oco_linked":
      return `group #${e.group} · #${e.first} ↔ #${e.second}`;
    case "twap_opened":
      return `${e.payload.side} ${e.payload.total_lots} lots · ${e.payload.slices} slices · every ${Math.round(e.payload.slice_interval_ms / 1000)}s`;
    case "twap_sliced":
      return `parent #${e.parent_id} · slice ${e.slice_index + 1}`;
    case "twap_closed":
      return `parent #${e.parent_id} ${e.reason} · ${e.placed_lots} lots placed`;
    case "vol_index_published":
      return `${e.base_symbol} vol index ${(e.index_permille / 10).toFixed(1)}%`;
    case "collateral_interest_accrued":
      return `${e.currency} +${formatMoney(e.amount_minor, dec(e.currency))} = ${usd(e.quote_value_minor)}`;
    case "insurance_marked":
      return `${shortSymbol(e.symbol)} ${e.signed_lots} lots · ${pnl(e.pnl_quote_minor)}`;
    case "insurance_rebalanced":
      return `${shortSymbol(e.symbol)} ${e.lots} lots · ${usdCompact(e.proceeds_quote_minor)} proceeds`;
    case "vault_epoch_settled":
      return `vault #${e.payload.vault_id} epoch ${e.payload.epoch} · +${usdCompact(e.payload.insurance_credit_quote_minor)} insurance credit`;
    case "vault_opened":
      return `vault #${e.vault_id} · ${e.revenue_share_bps} bps to LPs`;
    case "vault_queued":
      return `vault #${e.vault_id} ${e.is_subscribe ? "subscribe" : "redeem"} ${usd(e.amount)} · sub ${e.subaccount}`;
    case "mm_enrolled":
      return `sub ${e.subaccount} enrolled into tier review`;
    case "mm_tier_adjusted":
      return `sub ${e.subaccount} → ${e.tier ?? "unranked"} · fee −${(e.fee_discount_bps / 100).toFixed(1)}%`;
    case "quote_interest_accrued":
      return `${usd(e.amount_quote_minor, { sign: true })} · sub ${e.subaccount}`;
  }
}
