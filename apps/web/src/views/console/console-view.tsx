"use client";

/**
 * The Console — the raw command surface. A power-user protocol console:
 * compose any of the 31 engine Commands field-by-field, fire it, and
 * watch submissions, engine errors, and the journal tail flow back in
 * the live log. Sensible defaults everywhere — most commands submit
 * in ≤2 clicks.
 */

import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useVenueStore } from "@/lib/venue-store";
import { getVenueClient } from "@perp/api-client";
import { shortSymbol, simTime, ticksToPrice, usd, usdCompact } from "@/lib/fmt";
import { tryParseMoney } from "@perp/types";
import type {
  AccountDescriptor,
  Command,
  CommandType,
  Event,
  Instrument,
  OrderRequest,
  OrderType,
  Side,
  TimeInForce,
  VenueSnapshot,
} from "@perp/types";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectLabel,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { RotateCcw, Send, TerminalSquare } from "lucide-react";

/* ─────────────────────────── command groups ─────────────────────────── */

const COMMAND_GROUPS: { label: string; types: CommandType[] }[] = [
  { label: "core", types: ["deposit", "withdraw", "transfer"] },
  {
    label: "orders",
    types: [
      "place",
      "cancel",
      "cancel_all",
      "amend",
      "place_batch",
      "cancel_batch",
      "place_oco",
      "place_twap",
      "cancel_twap",
      "exercise",
    ],
  },
  { label: "market", types: ["oracle_update", "tick", "begin_auction"] },
  { label: "rfq", types: ["rfq_create", "rfq_quote", "rfq_execute", "rfq_cancel", "block_trade"] },
  { label: "collateral", types: ["deposit_collateral", "withdraw_collateral", "convert_collateral"] },
  { label: "maker", types: ["set_mmp", "set_cod", "session_dropped", "mm_tier_enroll"] },
  { label: "vaults", types: ["vault_create", "vault_subscribe", "vault_redeem"] },
];

/* ─────────────────────────── form schema ─────────────────────────── */

type Vals = Record<string, string | boolean>;

type Field =
  | { key: string; label: string; type: "subaccount"; def?: number }
  | { key: string; label: string; type: "symbol" }
  | { key: string; label: string; type: "symbolAll" }
  | { key: string; label: string; type: "side"; def: Side }
  | { key: string; label: string; type: "number"; def: number; min?: number }
  | { key: string; label: string; type: "money"; def: string }
  | { key: string; label: string; type: "money8"; def: string }
  | { key: string; label: string; type: "int"; def: string; nullable?: boolean }
  | { key: string; label: string; type: "bool"; def: boolean }
  | { key: string; label: string; type: "enum"; options: readonly string[]; def: string }
  | { key: string; label: string; type: "ids"; def: string }
  | { key: string; label: string; type: "order" };

const SUB = { key: "sub", label: "subaccount", type: "subaccount" } as const;
const AMOUNT = { key: "amount", label: "amount_quote_minor (USD)", type: "money", def: "10000" } as const;

const FORMS: Record<CommandType, Field[]> = {
  deposit: [SUB, AMOUNT],
  withdraw: [SUB, { ...AMOUNT, def: "5000" }],
  transfer: [
    { key: "sub", label: "from", type: "subaccount" },
    { key: "to", label: "to", type: "subaccount", def: 3 },
    { key: "amount", label: "amount_quote_minor (USD)", type: "money", def: "1000" },
  ],
  place: [SUB, { key: "order", label: "OrderRequest", type: "order" }],
  cancel: [SUB, { key: "order_id", label: "order_id", type: "int", def: "1" }],
  cancel_all: [SUB, { key: "symbol", label: "symbol (all = null)", type: "symbolAll" }],
  oracle_update: [
    { key: "provider", label: "provider", type: "enum", options: ["pyth", "chainlink", "apis3"], def: "pyth" },
    { key: "price", label: "price_quote_minor (USD)", type: "money", def: "80000" },
  ],
  tick: [],
  rfq_create: [
    { key: "sub", label: "taker", type: "subaccount" },
    { key: "symbol", label: "leg symbol", type: "symbol" },
    { key: "side", label: "leg side", type: "side", def: "bid" },
    { key: "lots", label: "qty_lots", type: "number", def: 5, min: 1 },
    { key: "cp", label: "counterparty", type: "subaccount", def: 1 },
    { key: "min", label: "min_total_cost (USD · empty = null)", type: "money", def: "" },
    { key: "max", label: "max_total_cost (USD · empty = null)", type: "money", def: "" },
    { key: "ttl_s", label: "ttl (seconds)", type: "number", def: 30 },
  ],
  rfq_quote: [
    { key: "sub", label: "maker", type: "subaccount", def: 1 },
    { key: "rfq_id", label: "rfq_id", type: "int", def: "1" },
    { key: "leg_price", label: "leg_prices_ticks[0]", type: "number", def: 79500 },
    { key: "ttl_s", label: "ttl (seconds)", type: "number", def: 30 },
  ],
  rfq_execute: [
    { key: "sub", label: "taker", type: "subaccount" },
    { key: "rfq_id", label: "rfq_id", type: "int", def: "1" },
    { key: "quote_id", label: "quote_id", type: "int", def: "1" },
  ],
  rfq_cancel: [
    SUB,
    { key: "rfq_id", label: "rfq_id (empty = null)", type: "int", def: "", nullable: true },
    { key: "quote_id", label: "quote_id (empty = null)", type: "int", def: "", nullable: true },
  ],
  block_trade: [
    { key: "sub", label: "taker", type: "subaccount" },
    { key: "maker", label: "maker", type: "subaccount", def: 1 },
    { key: "symbol", label: "leg symbol", type: "symbol" },
    { key: "side", label: "leg side", type: "side", def: "bid" },
    { key: "lots", label: "qty_lots", type: "number", def: 10, min: 1 },
    { key: "leg_price", label: "leg price_ticks", type: "number", def: 79500 },
  ],
  exercise: [
    SUB,
    { key: "symbol", label: "symbol (american)", type: "symbol" },
    { key: "lots", label: "lots", type: "number", def: 1, min: 1 },
  ],
  set_mmp: [
    SUB,
    { key: "interval_ms", label: "interval_ms", type: "number", def: 5000 },
    { key: "frozen_ms", label: "frozen_time_ms", type: "number", def: 1000 },
    { key: "amount_limit_lots", label: "amount_limit_lots", type: "number", def: 100 },
    { key: "delta_limit_lots", label: "delta_limit_lots", type: "number", def: 50 },
  ],
  set_cod: [SUB, { key: "enabled", label: "enabled", type: "bool", def: true }],
  session_dropped: [SUB],
  place_batch: [
    SUB,
    { key: "order", label: "OrderRequest", type: "order" },
    { key: "count", label: "duplicate × count", type: "number", def: 3, min: 1 },
  ],
  cancel_batch: [SUB, { key: "ids", label: "order_ids (comma-separated)", type: "ids", def: "1" }],
  amend: [
    SUB,
    { key: "order_id", label: "order_id", type: "int", def: "1" },
    { key: "new_price_ticks", label: "new_price_ticks (empty = null)", type: "int", def: "", nullable: true },
    { key: "new_open_lots", label: "new_open_lots (empty = null)", type: "int", def: "", nullable: true },
  ],
  begin_auction: [
    { key: "symbol", label: "symbol", type: "symbol" },
    { key: "duration_s", label: "uncross in (seconds)", type: "number", def: 120 },
  ],
  deposit_collateral: [
    SUB,
    { key: "currency", label: "currency", type: "enum", options: ["BTC"], def: "BTC" },
    { key: "amount", label: "amount_minor (BTC)", type: "money8", def: "0.1" },
  ],
  withdraw_collateral: [
    SUB,
    { key: "currency", label: "currency", type: "enum", options: ["BTC"], def: "BTC" },
    { key: "amount", label: "amount_minor (BTC)", type: "money8", def: "0.1" },
  ],
  convert_collateral: [
    SUB,
    { key: "from", label: "from", type: "enum", options: ["USD", "BTC"], def: "USD" },
    { key: "to", label: "to", type: "enum", options: ["BTC", "USD"], def: "BTC" },
    { key: "amount", label: "from_amount (units of from)", type: "money", def: "1000" },
  ],
  place_oco: [
    SUB,
    { key: "order", label: "first OrderRequest", type: "order" },
    { key: "side2", label: "second side", type: "side", def: "ask" },
    { key: "lots2", label: "second qty_lots", type: "number", def: 5, min: 1 },
    { key: "price_ticks2", label: "second price_ticks", type: "number", def: 80500 },
  ],
  place_twap: [
    SUB,
    { key: "symbol", label: "symbol", type: "symbol" },
    { key: "side", label: "side", type: "side", def: "bid" },
    { key: "total_lots", label: "total_lots", type: "number", def: 100, min: 1 },
    { key: "slices", label: "slices", type: "number", def: 10, min: 1 },
    { key: "interval_s", label: "slice interval (seconds)", type: "number", def: 30 },
    { key: "limit_ticks", label: "limit_ticks (empty = null)", type: "int", def: "", nullable: true },
  ],
  cancel_twap: [SUB, { key: "parent_id", label: "parent_id", type: "int", def: "1" }],
  vault_create: [{ key: "revenue_share_bps", label: "revenue_share_bps", type: "number", def: 1000 }],
  vault_subscribe: [
    SUB,
    { key: "vault_id", label: "vault_id", type: "int", def: "1" },
    { key: "amount", label: "amount_quote_minor (USD)", type: "money", def: "5000" },
  ],
  vault_redeem: [
    SUB,
    { key: "vault_id", label: "vault_id", type: "int", def: "1" },
    { key: "shares", label: "shares (bigint)", type: "int", def: "10" },
  ],
  mm_tier_enroll: [SUB],
};

interface Ctx {
  now: number;
  activeAccount: number;
  activeSymbol: string;
  descriptors: AccountDescriptor[];
  instruments: Instrument[];
  snapshot: VenueSnapshot | null;
}

/* ─────────────────────────── build helpers ─────────────────────────── */

class BuildError extends Error {}

const str = (v: Vals, key: string, fallback = ""): string => String(v[key] ?? fallback).trim();

function needMoney(v: Vals, key: string, label: string, opts: { nullable?: boolean; decimals?: number } = {}): bigint | null {
  const raw = str(v, key);
  if (raw === "") {
    if (opts.nullable) return null;
    throw new BuildError(`${label}: enter an amount`);
  }
  const m = tryParseMoney(raw, opts.decimals ?? 2);
  if (m == null) throw new BuildError(`${label}: invalid amount "${raw}"`);
  return m;
}

function needInt(v: Vals, key: string, label: string, opts: { nullable?: boolean; positive?: boolean } = {}): number | null {
  const raw = str(v, key);
  if (raw === "") {
    if (opts.nullable) return null;
    throw new BuildError(`${label}: enter an integer`);
  }
  if (!/^-?\d+$/.test(raw)) throw new BuildError(`${label}: invalid integer "${raw}"`);
  const n = Number(raw);
  if (!Number.isSafeInteger(n)) throw new BuildError(`${label}: out of range`);
  if (opts.positive && n <= 0) throw new BuildError(`${label}: must be > 0`);
  return n;
}

function needBig(v: Vals, key: string, label: string, opts: { nullable?: boolean } = {}): bigint | null {
  const raw = str(v, key);
  if (raw === "") {
    if (opts.nullable) return null;
    throw new BuildError(`${label}: enter an integer`);
  }
  if (!/^\d+$/.test(raw)) throw new BuildError(`${label}: invalid bigint "${raw}"`);
  return BigInt(raw);
}

function needNum(v: Vals, key: string, label: string, def: number): number {
  const raw = str(v, key);
  if (raw === "") return def;
  const n = Number(raw);
  if (!Number.isFinite(n)) throw new BuildError(`${label}: invalid number "${raw}"`);
  return n;
}

function needLots(v: Vals, key: string, label: string, def: number): number {
  const n = needNum(v, key, label, def);
  if (!Number.isInteger(n) || n <= 0) throw new BuildError(`${label}: must be a positive integer`);
  return n;
}

function sideOf(v: Vals, key: string): Side {
  return str(v, key, "bid") === "ask" ? "ask" : "bid";
}

function ticksOfPrice(v: Vals, key: string, label: string, inst: Instrument | undefined): number {
  const minor = needMoney(v, key, label);
  if (minor == null || minor <= 0n) throw new BuildError(`${label}: must be > 0`);
  if (!inst) throw new BuildError("unknown instrument");
  return Number(minor / inst.tick_size_quote_minor);
}

function buildOrderRequest(v: Vals, ctx: Ctx, sub: number): OrderRequest {
  const symbol = str(v, "o_symbol", ctx.activeSymbol) || ctx.activeSymbol;
  const inst = ctx.instruments.find((i) => i.symbol === symbol);
  if (!inst) throw new BuildError(`unknown instrument "${symbol}"`);
  const side = sideOf(v, "o_side");
  const qty_lots = needLots(v, "o_lots", "qty_lots", 5);

  const kind = str(v, "o_kind", "limit") || "limit";
  let order_type: OrderType;
  let price_ticks: number | null = null;
  switch (kind) {
    case "limit":
      price_ticks = ticksOfPrice(v, "o_price", "limit price", inst);
      order_type = { kind: "limit" };
      break;
    case "market":
      order_type = { kind: "market" };
      break;
    case "stop_market":
      order_type = { kind: "stop_market", trigger_price: ticksOfPrice(v, "o_trigger", "trigger price", inst) };
      break;
    case "stop_limit":
      order_type = {
        kind: "stop_limit",
        trigger_price: ticksOfPrice(v, "o_trigger", "trigger price", inst),
        limit_price: ticksOfPrice(v, "o_limitprice", "limit price", inst),
      };
      price_ticks = order_type.limit_price;
      break;
    case "trailing_stop_market": {
      const offset = needInt(v, "o_offset", "offset_ticks", { positive: true }) ?? 0;
      order_type = { kind: "trailing_stop_market", offset_ticks: offset };
      break;
    }
    case "trailing_stop_limit": {
      const offset = needInt(v, "o_offset", "offset_ticks", { positive: true }) ?? 0;
      const limit = needInt(v, "o_limitOffset", "limit_ticks", { positive: true }) ?? 0;
      order_type = { kind: "trailing_stop_limit", offset_ticks: offset, limit_ticks: limit };
      break;
    }
    default:
      throw new BuildError(`unknown order kind "${kind}"`);
  }

  const tifKind = str(v, "o_tif", "gtc") || "gtc";
  let tif: TimeInForce;
  if (tifKind === "gtd") {
    const seconds = needNum(v, "o_gtd_s", "gtd until (s)", 300);
    tif = { kind: "gtd", until: ctx.now + Math.round(seconds * 1000) };
  } else if (tifKind === "ioc" || tifKind === "fok") {
    tif = { kind: tifKind };
  } else {
    tif = { kind: "gtc" };
  }

  return {
    subaccount: sub,
    symbol,
    side,
    order_type,
    price_ticks,
    qty_lots,
    tif,
    post_only: v.o_post === true,
    reduce_only: v.o_ro === true,
    stp: "cancel_newest",
    display_lots: null,
    oco_group: null,
    client_ts: 0,
  };
}

function buildCommand(type: CommandType, v: Vals, ctx: Ctx): { ok: true; command: Command } | { ok: false; error: string } {
  try {
    return { ok: true, command: buildCommandInner(type, v, ctx) };
  } catch (err) {
    if (err instanceof BuildError) return { ok: false, error: err.message };
    return { ok: false, error: err instanceof Error ? err.message : String(err) };
  }
}

function buildCommandInner(type: CommandType, v: Vals, ctx: Ctx): Command {
  const now = ctx.now;
  const sub = needInt(v, "sub", "subaccount") ?? ctx.activeAccount;

  switch (type) {
    case "deposit":
      return {
        type: "deposit",
        subaccount: sub,
        amount_quote_minor: needMoney(v, "amount", "amount") ?? 0n,
      };
    case "withdraw":
      return {
        type: "withdraw",
        subaccount: sub,
        amount_quote_minor: needMoney(v, "amount", "amount") ?? 0n,
      };
    case "place":
      return { type: "place", request: buildOrderRequest(v, ctx, sub), now };
    case "cancel":
      return {
        type: "cancel",
        subaccount: sub,
        order_id: needInt(v, "order_id", "order_id", { positive: true }) ?? 1,
        now,
      };
    case "cancel_all": {
      const symbol = str(v, "symbol", "all");
      return { type: "cancel_all", subaccount: sub, symbol: symbol === "all" || symbol === "" ? null : symbol, now };
    }
    case "oracle_update":
      return {
        type: "oracle_update",
        base_symbol: "BTC",
        provider: str(v, "provider", "pyth") || "pyth",
        ts: now,
        price_quote_minor: needMoney(v, "price", "price") ?? 0n,
      };
    case "tick":
      return { type: "tick", now };
    case "rfq_create": {
      const ttl = Math.round(needNum(v, "ttl_s", "ttl", 30) * 1000);
      return {
        type: "rfq_create",
        taker: sub,
        legs: [
          {
            symbol: str(v, "symbol", ctx.activeSymbol) || ctx.activeSymbol,
            side: sideOf(v, "side"),
            qty_lots: needLots(v, "lots", "qty_lots", 5),
          },
        ],
        counterparties: [needInt(v, "cp", "counterparty") ?? 1],
        min_total_cost_quote_minor: needMoney(v, "min", "min_total_cost", { nullable: true }),
        max_total_cost_quote_minor: needMoney(v, "max", "max_total_cost", { nullable: true }),
        ttl_ms: ttl,
        now,
      };
    }
    case "rfq_quote": {
      const legPrice = needInt(v, "leg_price", "leg_prices_ticks[0]", { positive: true }) ?? 0;
      return {
        type: "rfq_quote",
        maker: sub,
        rfq_id: needInt(v, "rfq_id", "rfq_id", { positive: true }) ?? 1,
        leg_prices_ticks: [legPrice],
        ttl_ms: Math.round(needNum(v, "ttl_s", "ttl", 30) * 1000),
        now,
      };
    }
    case "rfq_execute":
      return {
        type: "rfq_execute",
        taker: sub,
        rfq_id: needInt(v, "rfq_id", "rfq_id", { positive: true }) ?? 1,
        quote_id: needInt(v, "quote_id", "quote_id", { positive: true }) ?? 1,
        now,
      };
    case "rfq_cancel":
      return {
        type: "rfq_cancel",
        subaccount: sub,
        rfq_id: needInt(v, "rfq_id", "rfq_id", { nullable: true }),
        quote_id: needInt(v, "quote_id", "quote_id", { nullable: true }),
        now,
      };
    case "block_trade": {
      const symbol = str(v, "symbol", ctx.activeSymbol) || ctx.activeSymbol;
      const side = sideOf(v, "side");
      const qty_lots = needLots(v, "lots", "qty_lots", 10);
      const price_ticks = needInt(v, "leg_price", "leg price_ticks", { positive: true }) ?? 1;
      return {
        type: "block_trade",
        taker: sub,
        maker: needInt(v, "maker", "maker") ?? 1,
        legs: [{ symbol, side, qty_lots, price_ticks }],
        now,
      };
    }
    case "transfer":
      return {
        type: "transfer",
        from: sub,
        to: needInt(v, "to", "to") ?? 3,
        amount_quote_minor: needMoney(v, "amount", "amount") ?? 0n,
        now,
      };
    case "exercise":
      return {
        type: "exercise",
        subaccount: sub,
        symbol: str(v, "symbol", ctx.activeSymbol) || ctx.activeSymbol,
        lots: needLots(v, "lots", "lots", 1),
        now,
      };
    case "set_mmp":
      return {
        type: "set_mmp",
        subaccount: sub,
        base_symbol: "BTC",
        interval_ms: needNum(v, "interval_ms", "interval_ms", 5000),
        frozen_time_ms: needNum(v, "frozen_ms", "frozen_time_ms", 1000),
        amount_limit_lots: needNum(v, "amount_limit_lots", "amount_limit_lots", 100),
        delta_limit_lots: needNum(v, "delta_limit_lots", "delta_limit_lots", 50),
        now,
      };
    case "set_cod":
      return { type: "set_cod", subaccount: sub, enabled: v.enabled !== false, now };
    case "session_dropped":
      return { type: "session_dropped", subaccount: sub, now };
    case "place_batch": {
      const request = buildOrderRequest(v, ctx, sub);
      const count = needLots(v, "count", "count", 3);
      return { type: "place_batch", requests: Array.from({ length: count }, () => ({ ...request })), now };
    }
    case "cancel_batch": {
      const raw = str(v, "ids", "1");
      const ids = raw
        .split(",")
        .map((s) => s.trim())
        .filter((s) => s !== "")
        .map((s) => {
          const n = Number(s);
          if (!Number.isInteger(n) || n <= 0) throw new BuildError(`order_ids: invalid id "${s}"`);
          return n;
        });
      if (ids.length === 0) throw new BuildError("order_ids: enter at least one id");
      return { type: "cancel_batch", subaccount: sub, order_ids: ids, now };
    }
    case "amend":
      return {
        type: "amend",
        subaccount: sub,
        order_id: needInt(v, "order_id", "order_id", { positive: true }) ?? 1,
        new_price_ticks: needInt(v, "new_price_ticks", "new_price_ticks", { nullable: true }),
        new_open_lots: needInt(v, "new_open_lots", "new_open_lots", { nullable: true }),
        now,
      };
    case "begin_auction":
      return {
        type: "begin_auction",
        symbol: str(v, "symbol", ctx.activeSymbol) || ctx.activeSymbol,
        uncross_at: now + Math.round(needNum(v, "duration_s", "duration", 120) * 1000),
        now,
      };
    case "deposit_collateral":
      return {
        type: "deposit_collateral",
        subaccount: sub,
        currency: str(v, "currency", "BTC") || "BTC",
        amount_minor: needMoney(v, "amount", "amount", { decimals: 8 }) ?? 0n,
        now,
      };
    case "withdraw_collateral":
      return {
        type: "withdraw_collateral",
        subaccount: sub,
        currency: str(v, "currency", "BTC") || "BTC",
        amount_minor: needMoney(v, "amount", "amount", { decimals: 8 }) ?? 0n,
        now,
      };
    case "convert_collateral":
      return {
        type: "convert_collateral",
        subaccount: sub,
        from: str(v, "from", "USD") || "USD",
        to: str(v, "to", "BTC") || "BTC",
        from_amount_minor: needMoney(v, "amount", "from_amount", {
          decimals: str(v, "from", "USD") === "BTC" ? 8 : 2,
        }) ?? 0n,
        now,
      };
    case "place_oco": {
      const first = buildOrderRequest(v, ctx, sub);
      const side2 = sideOf(v, "side2");
      return {
        type: "place_oco",
        first,
        second: {
          ...first,
          side: side2,
          qty_lots: needLots(v, "lots2", "second qty_lots", 5),
          order_type: { kind: "limit" },
          price_ticks: needInt(v, "price_ticks2", "second price_ticks", { positive: true }) ?? 1,
        },
        now,
      };
    }
    case "place_twap":
      return {
        type: "place_twap",
        subaccount: sub,
        symbol: str(v, "symbol", ctx.activeSymbol) || ctx.activeSymbol,
        side: sideOf(v, "side"),
        total_lots: needLots(v, "total_lots", "total_lots", 100),
        slices: needLots(v, "slices", "slices", 10),
        slice_interval_ms: Math.round(needNum(v, "interval_s", "interval", 30) * 1000),
        limit_ticks: needInt(v, "limit_ticks", "limit_ticks", { nullable: true }),
        now,
      };
    case "cancel_twap":
      return {
        type: "cancel_twap",
        subaccount: sub,
        parent_id: needInt(v, "parent_id", "parent_id", { positive: true }) ?? 1,
        now,
      };
    case "vault_create":
      return {
        type: "vault_create",
        revenue_share_bps: needNum(v, "revenue_share_bps", "revenue_share_bps", 1000),
        now,
      };
    case "vault_subscribe":
      return {
        type: "vault_subscribe",
        vault_id: needInt(v, "vault_id", "vault_id", { positive: true }) ?? 1,
        subaccount: sub,
        amount_quote_minor: needMoney(v, "amount", "amount") ?? 0n,
        now,
      };
    case "vault_redeem":
      return {
        type: "vault_redeem",
        vault_id: needInt(v, "vault_id", "vault_id", { positive: true }) ?? 1,
        subaccount: sub,
        shares: needBig(v, "shares", "shares") ?? 1n,
        now,
      };
    case "mm_tier_enroll":
      return { type: "mm_tier_enroll", subaccount: sub, now };
    default: {
      const exhausted: never = type;
      throw new BuildError(`unknown command "${String(exhausted)}"`);
    }
  }
}

/* ─────────────────────────── defaults ─────────────────────────── */

function markOf(ctx: Ctx, symbol: string): number | null {
  const m = ctx.snapshot?.markets.find((x) => x.symbol === symbol)?.mark_quote_minor;
  return m != null ? Number(m) / 100 : null;
}

function orderDefaults(ctx: Ctx): Vals {
  const mark = markOf(ctx, ctx.activeSymbol);
  return {
    o_symbol: ctx.activeSymbol,
    o_side: "bid",
    o_lots: "5",
    o_kind: "limit",
    o_price: mark != null ? mark.toFixed(0) : "79000",
    o_trigger: mark != null ? (mark - 500).toFixed(0) : "78500",
    o_limitprice: mark != null ? (mark - 450).toFixed(0) : "78600",
    o_offset: "100",
    o_limitOffset: "50",
    o_tif: "gtc",
    o_gtd_s: "300",
    o_post: false,
    o_ro: false,
  };
}

function intDefault(key: string, ctx: Ctx): string | null {
  const snap = ctx.snapshot;
  const active = ctx.activeAccount;
  if (!snap) return null;
  switch (key) {
    case "order_id": {
      const o = snap.openOrders[active]?.[0];
      return o ? String(o.order_id) : null;
    }
    case "rfq_id": {
      const r = [...snap.rfqs].reverse().find((x) => x.status === "open" || x.status === "quoted");
      return r ? String(r.rfq_id) : null;
    }
    case "quote_id": {
      const r = [...snap.rfqs].reverse().find((x) => x.status === "quoted");
      const q = r?.quotes[r.quotes.length - 1];
      return q ? String(q.quote_id) : null;
    }
    case "parent_id": {
      const t = snap.twaps.find((x) => x.state === "running");
      return t ? String(t.parent_id) : null;
    }
    case "vault_id": {
      const vault = snap.vaults[0];
      return vault ? String(vault.vault_id) : null;
    }
    default:
      return null;
  }
}

function defaultsFor(type: CommandType, ctx: Ctx): Vals {
  const v: Vals = {};
  for (const f of FORMS[type]) {
    switch (f.type) {
      case "subaccount":
        v[f.key] = String(f.def ?? ctx.activeAccount);
        break;
      case "symbol":
        v[f.key] =
          type === "exercise"
            ? (ctx.instruments.find((i) => i.kind === "option" && i.exercise_style === "american")?.symbol ??
              ctx.activeSymbol)
            : ctx.activeSymbol;
        break;
      case "symbolAll":
        v[f.key] = "all";
        break;
      case "side":
        v[f.key] = f.def;
        break;
      case "number":
        v[f.key] = String(f.def);
        break;
      case "money":
      case "money8":
        v[f.key] = f.def;
        break;
      case "int":
        v[f.key] = intDefault(f.key, ctx) ?? f.def;
        break;
      case "bool":
        v[f.key] = f.def;
        break;
      case "enum":
        v[f.key] = f.def;
        break;
      case "ids":
        v[f.key] = f.def;
        break;
      case "order":
        Object.assign(v, orderDefaults(ctx));
        break;
    }
  }
  return v;
}

function countLeaves(x: unknown): number {
  if (x == null || typeof x !== "object") return x == null ? 0 : 1;
  if (Array.isArray(x)) return x.reduce((a, item) => a + countLeaves(item), 0);
  return Object.values(x).reduce((a, item) => a + countLeaves(item), 0);
}

/* ─────────────────────────── mini event summarizer (log) ─────────────────────────── */

function miniSummary(e: Event, instOf: (symbol: string) => Instrument | undefined): string {
  const px = (symbol: string, ticks: number): string => ticksToPrice(ticks, instOf(symbol) ?? undefined);
  switch (e.type) {
    case "trade_executed":
      return `×${e.payload.qty_lots} @ ${px(e.payload.symbol, e.payload.price_ticks)} · ${usdCompact(e.payload.notional_quote_minor)}`;
    case "deposit":
      return `+${usd(e.amount_quote_minor)} sub ${e.subaccount}`;
    case "withdrawal":
      return `−${usd(e.amount_quote_minor)} sub ${e.subaccount}`;
    case "withdraw_rejected":
      return `${usd(e.requested)} · ${e.reason}`;
    case "order_rejection":
      return `reason: ${e.payload.reason.kind}`;
    case "order_resting":
      return `#${e.order.id} ${e.order.qty_lots} lots resting`;
    case "order_closed":
      return `#${e.order_id} ${e.reason}`;
    case "funding":
      return `${e.payload.rate_bps.toFixed(1)}bp per interval`;
    case "funding_flow":
      return `${usd(e.payload.credit_quote_minor, { sign: true })} sub ${e.payload.subaccount}`;
    case "liquidation":
      return `${e.payload.lots} lots · ${e.payload.to_insurance ? "to insurance" : "to book"}`;
    case "adl":
      return `sub ${e.payload.liquidated_subaccount} ↔ sub ${e.payload.counterparty_subaccount}`;
    case "provider_observed":
      return `${e.provider} ${usd(e.price_quote_minor)}`;
    case "transfer_executed":
      return `${usd(e.amount_quote_minor)} sub ${e.from} → sub ${e.to}`;
    case "reward":
      return `+${usd(e.payload.amount_quote_minor)} sub ${e.payload.subaccount}`;
    case "rfq_settled":
      return `${e.trades.length} legs executed`;
    case "vault_queued":
      return `vault #${e.vault_id} ${e.is_subscribe ? "sub" : "redeem"} ${usd(e.amount)}`;
    case "option_exercised":
      return `${e.payload.settled_lots}/${e.payload.requested_lots} lots`;
    default:
      return genericMini(e);
  }
}

function genericMini(e: Event): string {
  const parts: string[] = [];
  for (const [k, val] of Object.entries(e as unknown as Record<string, unknown>)) {
    if (k === "type") continue;
    if (typeof val === "number") parts.push(`${k}=${val}`);
    else if (typeof val === "bigint") parts.push(`${k}=${val}n`);
    else if (typeof val === "string" && val.length <= 20) parts.push(`${k}=${val}`);
    if (parts.length >= 3) break;
  }
  return parts.length > 0 ? parts.join(" ") : e.type;
}

const LOG_CAT_CLASS: Record<string, string> = {
  trade_executed: "text-up",
  rfq_settled: "text-up",
  block_registered: "text-up",
  block_printed: "text-up",
  liquidation: "text-down",
  adl: "text-down",
  breaker_tripped: "text-down",
  market_halted: "text-down",
  withdraw_rejected: "text-down",
  transfer_rejected: "text-down",
  collateral_rejected: "text-down",
  exercise_rejected: "text-down",
  rfq_rejected: "text-down",
  order_rejection: "text-down",
  funding: "text-chart-3",
  funding_flow: "text-chart-3",
  option_expiry: "text-chart-3",
  option_exercised: "text-chart-3",
};

/* ─────────────────────────── the view ─────────────────────────── */

const LOG_CAP = 300;

interface LogLine {
  id: number;
  ts: number;
  kind: "cmd" | "error" | "event";
  eventType?: string;
  text: string;
}

export const ConsoleView = memo(function ConsoleView() {
  const snapshot = useVenueStore((s) => s.snapshot);
  const activeAccount = useVenueStore((s) => s.activeAccount);
  const activeSymbol = useVenueStore((s) => s.activeSymbol);
  const journal = useVenueStore((s) => s.journal);
  const lastError = useVenueStore((s) => s.lastError);
  const markErrorSeen = useVenueStore((s) => s.markErrorSeen);

  const [cmdType, setCmdType] = useState<CommandType>("deposit");
  const [values, setValues] = useState<Vals>(() => ({}));

  const instMap = useMemo(() => {
    const m = new Map<string, Instrument>();
    for (const i of snapshot?.instruments ?? []) m.set(i.symbol, i);
    return m;
  }, [snapshot]);
  const instOf = useCallback((symbol: string) => instMap.get(symbol), [instMap]);

  const ctx = useMemo<Ctx>(
    () => ({
      now: snapshot?.meta.now ?? Date.now(),
      activeAccount,
      activeSymbol,
      descriptors: snapshot?.accountDescriptors ?? [],
      instruments: snapshot?.instruments ?? [],
      snapshot: snapshot ?? null,
    }),
    [snapshot, activeAccount, activeSymbol],
  );
  const ctxRef = useRef<Ctx>(ctx);
  // Keep the ref fresh for late readers (submit / reset / re-seed) without
  // clobbering the form on every snapshot — sync happens post-render.
  useEffect(() => {
    ctxRef.current = ctx;
  });

  // Re-seed the form whenever the command type changes.
  useEffect(() => {
    setValues(defaultsFor(cmdType, ctxRef.current));
  }, [cmdType]);

  /* ── the live log ── */
  const [lines, setLines] = useState<LogLine[]>([]);
  const lineIdRef = useRef(0);
  const pushLines = useCallback((ls: Omit<LogLine, "id">[]) => {
    if (ls.length === 0) return;
    setLines((prev) => {
      const next = [...prev, ...ls.map((l) => ({ ...l, id: lineIdRef.current++ }))];
      return next.length > LOG_CAP ? next.slice(-LOG_CAP) : next;
    });
  }, []);

  // journal tail → log
  const journalSeqRef = useRef(0);
  useEffect(() => {
    const lastSeq = journal.length > 0 ? journal[journal.length - 1]!.seq : 0;
    if (lastSeq < journalSeqRef.current) journalSeqRef.current = 0; // engine reset
    const fresh = journal.filter((j) => j.seq > journalSeqRef.current);
    if (fresh.length === 0) return;
    journalSeqRef.current = fresh[fresh.length - 1]!.seq;
    pushLines(
      fresh.slice(-24).map((j) => ({
        ts: j.ts,
        kind: "event" as const,
        eventType: j.event.type,
        text: miniSummary(j.event, instOf),
      })),
    );
    if (fresh.length > 24) {
      pushLines([{ ts: fresh[0]!.ts, kind: "event", eventType: undefined, text: `… ${fresh.length - 24} more events elided` }]);
    }
  }, [journal, instOf, pushLines]);

  // engine errors → log
  const seenErrorRef = useRef<string | null>(null);
  useEffect(() => {
    if (!lastError || lastError === seenErrorRef.current) return;
    seenErrorRef.current = lastError;
    pushLines([{ ts: Date.now(), kind: "error", text: lastError }]);
    markErrorSeen();
  }, [lastError, markErrorSeen, pushLines]);

  // auto-scroll the log while pinned to the bottom
  const logRef = useRef<HTMLDivElement | null>(null);
  const pinnedRef = useRef(true);
  useEffect(() => {
    const el = logRef.current;
    if (el && pinnedRef.current) el.scrollTop = el.scrollHeight;
  }, [lines]);

  /* ── dispatch ── */
  const sendCommand = useCallback(
    (command: Command) => {
      getVenueClient().send({ type: "command", command });
      pushLines([{ ts: ctxRef.current.now, kind: "cmd", text: `> ${command.type} ${countLeaves(command)} fields` }]);
    },
    [pushLines],
  );

  const submit = () => {
    const res = buildCommand(cmdType, values, ctxRef.current);
    if (!res.ok) {
      toast.error(`Command::${cmdType} — invalid`, { description: res.error });
      return;
    }
    sendCommand(res.command);
    toast.success(`Command::${cmdType} dispatched`, { description: "accepted → watch the journal log" });
  };

  /* ── presets ── */
  const presetDeposit = () =>
    sendCommand({ type: "deposit", subaccount: activeAccount, amount_quote_minor: 100_000_00n });
  const presetOracle = () => {
    getVenueClient().send({ type: "oracle_inject", provider: "apis3", price_quote_minor: 160_000_00n });
    pushLines([{ ts: ctx.now, kind: "cmd", text: "> oracle_inject apis3 $160,000.00 (quarantine demo)" }]);
  };
  const presetPor = () => {
    getVenueClient().send({ type: "por_build" });
    pushLines([{ ts: ctx.now, kind: "cmd", text: "> por_build · building nonce-bound liability tree" }]);
  };
  const presetTwap = () =>
    sendCommand({
      type: "place_twap",
      subaccount: activeAccount,
      symbol: activeSymbol,
      side: "bid",
      total_lots: 100,
      slices: 10,
      slice_interval_ms: 30_000,
      limit_ticks: null,
      now: ctx.now,
    });

  const fields = FORMS[cmdType];

  return (
    <div className="h-full flex flex-col min-h-0">
      {/* toolbar + presets */}
      <div className="h-11 shrink-0 flex items-center gap-2.5 px-3 border-b border-hairline overflow-x-auto scroll-thin">
        <TerminalSquare className="w-3.5 h-3.5 text-primary shrink-0" />
        <h2 className="text-[12px] font-semibold tracking-tight shrink-0">Console</h2>
        <span className="text-[9px] uppercase tracking-wider text-muted-foreground/60 font-mono shrink-0 hidden md:inline">
          raw command surface · 31 commands
        </span>
        <div className="flex-1" />
        <Button variant="secondary" size="sm" className="h-7 text-[10px] shrink-0" onClick={presetDeposit}>
          Deposit $100k
        </Button>
        <Button
          variant="secondary"
          size="sm"
          className="h-7 text-[10px] shrink-0 text-chart-3"
          onClick={presetOracle}
          title="Watch the 3-provider oracle quarantine the outlier"
        >
          Oracle 160k · apis3
        </Button>
        <Button variant="secondary" size="sm" className="h-7 text-[10px] shrink-0" onClick={presetPor}>
          Build PoR
        </Button>
        <Button
          variant="secondary"
          size="sm"
          className="h-7 text-[10px] shrink-0"
          onClick={presetTwap}
          title="100 lots bid · 10 slices · 30s apart (5 min)"
        >
          TWAP buy 100 · 10 slices · 5min
        </Button>
      </div>

      {/* two-pane */}
      <div className="flex-1 min-h-0 flex flex-col lg:flex-row gap-3 p-3">
        {/* composer */}
        <section className="panel flex flex-col min-h-0 lg:w-[400px] shrink-0 max-h-[55%] lg:max-h-none" aria-label="Command composer">
          <div className="flex items-center justify-between px-3.5 py-2.5 border-b border-hairline">
            <h3 className="text-[9px] uppercase tracking-wider text-muted-foreground">Command composer</h3>
            <span className="text-[9px] font-mono text-muted-foreground/50">Command::</span>
          </div>
          <div className="flex-1 min-h-0 overflow-auto scroll-thin p-3 flex flex-col gap-2.5">
            <Select value={cmdType} onValueChange={(v) => setCmdType(v as CommandType)}>
              <SelectTrigger size="sm" className="h-8 w-full text-[11px] font-mono bg-transparent border-hairline">
                <SelectValue placeholder="command type" />
              </SelectTrigger>
              <SelectContent className="max-h-80">
                {COMMAND_GROUPS.map((g) => (
                  <SelectGroup key={g.label}>
                    <SelectLabel className="text-[9px] uppercase tracking-wider">{g.label}</SelectLabel>
                    {g.types.map((t) => (
                      <SelectItem key={t} value={t} className="text-[11px] font-mono">
                        {t}
                      </SelectItem>
                    ))}
                  </SelectGroup>
                ))}
              </SelectContent>
            </Select>

            {fields.length === 0 && (
              <p className="text-[10.5px] text-muted-foreground/60 font-mono py-3 text-center">
                no fields — sends Command::tick to the engine clock
              </p>
            )}

            <div className="grid grid-cols-2 gap-2.5">
              {fields.map((f) => (
                <FieldControl
                  key={f.key}
                  field={f}
                  value={values[f.key]}
                  ctx={ctx}
                  onChange={(val) => setValues((prev) => ({ ...prev, [f.key]: val }))}
                  onOrderChange={(patch) => setValues((prev) => ({ ...prev, ...patch }))}
                  orderValues={values}
                />
              ))}
            </div>

            <div className="flex gap-2 mt-auto pt-2">
              <Button size="sm" className="h-8 flex-1 text-[11px] font-semibold" onClick={submit}>
                <Send className="w-3 h-3" />
                Send Command
              </Button>
              <Button
                variant="ghost"
                size="sm"
                className="h-8 text-[10px] text-muted-foreground border border-hairline"
                onClick={() => setValues(defaultsFor(cmdType, ctxRef.current))}
                aria-label="Reset form to defaults"
                title="Reset to defaults"
              >
                <RotateCcw className="w-3 h-3" />
              </Button>
            </div>
          </div>
        </section>

        {/* live log */}
        <section className="panel flex-1 min-w-0 flex flex-col min-h-0" aria-label="Live event and response log">
          <div className="flex items-center justify-between px-3.5 py-2.5 border-b border-hairline">
            <h3 className="text-[9px] uppercase tracking-wider text-muted-foreground">Live log — submissions · errors · journal tail</h3>
            <span className="flex items-center gap-1.5 text-[9px] font-mono text-muted-foreground/60">
              <span className={`w-1.5 h-1.5 rounded-full ${snapshot?.meta.running ? "bg-up pulse-dot" : "bg-muted-foreground/50"}`} />
              {lines.length}/{LOG_CAP} lines
            </span>
          </div>
          <div
            ref={logRef}
            onScroll={(e) => {
              const el = e.currentTarget;
              pinnedRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
            }}
            className="flex-1 min-h-0 overflow-auto scroll-thin p-2.5 font-mono text-[10px] leading-relaxed"
            role="log"
            aria-label="Console log"
          >
            {lines.length === 0 && (
              <p className="text-muted-foreground/40 text-[10.5px] p-4 text-center">
                quiet — send a command or preset and the protocol answers here
              </p>
            )}
            {lines.map((l) => (
              <div key={l.id} className="flex gap-2 items-baseline whitespace-nowrap">
                <span className="text-muted-foreground/40 shrink-0">{simTime(l.ts)}</span>
                {l.kind === "cmd" && (
                  <span className="text-primary shrink-0">{l.text.startsWith(">") ? l.text : `> ${l.text}`}</span>
                )}
                {l.kind === "error" && <span className="text-down shrink-0">! {l.text}</span>}
                {l.kind === "event" && (
                  <>
                    <span className={`shrink-0 ${LOG_CAT_CLASS[l.eventType ?? ""] ?? "text-muted-foreground/80"}`}>
                      {l.eventType ?? "…"}
                    </span>
                    <span className="text-muted-foreground/70 truncate">{l.text}</span>
                  </>
                )}
              </div>
            ))}
          </div>
        </section>
      </div>
    </div>
  );
});

/* ─────────────────────────── field controls ─────────────────────────── */

function FieldControl({
  field,
  value,
  ctx,
  onChange,
  onOrderChange,
  orderValues,
}: {
  field: Field;
  value: string | boolean | undefined;
  ctx: Ctx;
  onChange: (val: string | boolean) => void;
  onOrderChange: (patch: Vals) => void;
  orderValues: Vals;
}) {
  const wide = field.type === "money" || field.type === "money8" || field.type === "ids" || field.type === "order";

  if (field.type === "order") {
    return (
      <div className="col-span-2 border border-hairline/70 rounded-lg p-2.5 flex flex-col gap-2 bg-panel-2/40">
        <span className="text-[9px] uppercase tracking-wider text-muted-foreground">{field.label}</span>
        <OrderBuilder values={orderValues} ctx={ctx} onChange={onOrderChange} />
      </div>
    );
  }

  return (
    <label className={`flex flex-col gap-1 ${wide ? "col-span-2" : ""}`}>
      <span className="text-[9px] uppercase tracking-wider text-muted-foreground truncate" title={field.label}>
        {field.label}
      </span>
      {(() => {
        switch (field.type) {
          case "subaccount":
            return (
              <Select value={String(value ?? "")} onValueChange={onChange}>
                <SelectTrigger size="sm" className="h-8 w-full text-[11px] font-mono bg-transparent border-hairline">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent className="max-h-64">
                  {ctx.descriptors.map((d) => (
                    <SelectItem key={d.id} value={String(d.id)} className="text-[11px] font-mono">
                      {d.id} · {d.name}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            );
          case "symbol":
          case "symbolAll": {
            const isAll = field.type === "symbolAll";
            return (
              <Select value={String(value ?? "")} onValueChange={onChange}>
                <SelectTrigger size="sm" className="h-8 w-full text-[11px] font-mono bg-transparent border-hairline">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent className="max-h-64">
                  {isAll && (
                    <SelectItem value="all" className="text-[11px] font-mono">
                      all (null)
                    </SelectItem>
                  )}
                  {ctx.instruments.map((i) => (
                    <SelectItem key={i.symbol} value={i.symbol} className="text-[11px] font-mono">
                      {shortSymbol(i.symbol)}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            );
          }
          case "side":
            return (
              <Select value={String(value ?? "bid")} onValueChange={onChange}>
                <SelectTrigger size="sm" className="h-8 w-full text-[11px] font-mono bg-transparent border-hairline">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="bid" className="text-[11px] font-mono text-up">bid</SelectItem>
                  <SelectItem value="ask" className="text-[11px] font-mono text-down">ask</SelectItem>
                </SelectContent>
              </Select>
            );
          case "enum":
            return (
              <Select value={String(value ?? field.def)} onValueChange={onChange}>
                <SelectTrigger size="sm" className="h-8 w-full text-[11px] font-mono bg-transparent border-hairline">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {field.options.map((o) => (
                    <SelectItem key={o} value={o} className="text-[11px] font-mono">
                      {o}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            );
          case "bool":
            return (
              <span className="h-8 flex items-center gap-2">
                <Switch checked={value === true} onCheckedChange={onChange} aria-label={field.label} />
                <span className="text-[10px] font-mono text-muted-foreground">{value === true ? "true" : "false"}</span>
              </span>
            );
          default:
            return (
              <Input
                value={String(value ?? "")}
                onChange={(e) => onChange(e.target.value)}
                inputMode={field.type === "money" || field.type === "money8" ? "decimal" : "numeric"}
                className="h-8 text-[11px] font-mono bg-transparent border-hairline num"
                placeholder={field.type === "int" && field.nullable ? "null" : field.type === "ids" ? "1,2,3" : ""}
              />
            );
        }
      })()}
    </label>
  );
}

/* ─────────────────────────── order builder ─────────────────────────── */

const ORDER_KINDS = [
  "limit",
  "market",
  "stop_market",
  "stop_limit",
  "trailing_stop_market",
  "trailing_stop_limit",
] as const;

function OrderBuilder({
  values,
  ctx,
  onChange,
}: {
  values: Vals;
  ctx: Ctx;
  onChange: (patch: Vals) => void;
}) {
  const set = (key: string, val: string | boolean) => onChange({ [key]: val });
  const kind = String(values.o_kind ?? "limit");
  const tif = String(values.o_tif ?? "gtc");
  const inst = ctx.instruments.find((i) => i.symbol === String(values.o_symbol ?? ""));
  const tick = inst ? usd(inst.tick_size_quote_minor) : "—";

  return (
    <div className="grid grid-cols-2 gap-2">
      <FieldControl
        field={{ key: "o_symbol", label: "symbol", type: "symbol" }}
        value={values.o_symbol}
        ctx={ctx}
        onChange={(v) => set("o_symbol", v)}
        onOrderChange={onChange}
        orderValues={values}
      />
      <FieldControl
        field={{ key: "o_side", label: "side", type: "side", def: "bid" }}
        value={values.o_side}
        ctx={ctx}
        onChange={(v) => set("o_side", v)}
        onOrderChange={onChange}
        orderValues={values}
      />
      <label className="flex flex-col gap-1">
        <span className="text-[9px] uppercase tracking-wider text-muted-foreground">
          qty_lots{inst ? ` · 1 lot = ${Number(inst.lot_size_base_minor) / 10 ** inst.base_decimals} base` : ""}
        </span>
        <Input
          value={String(values.o_lots ?? "")}
          onChange={(e) => set("o_lots", e.target.value)}
          inputMode="numeric"
          className="h-8 text-[11px] font-mono bg-transparent border-hairline num"
        />
      </label>
      <label className="flex flex-col gap-1">
        <span className="text-[9px] uppercase tracking-wider text-muted-foreground">order_type.kind</span>
        <Select value={kind} onValueChange={(v) => set("o_kind", v)}>
          <SelectTrigger size="sm" className="h-8 w-full text-[11px] font-mono bg-transparent border-hairline">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {ORDER_KINDS.map((k) => (
              <SelectItem key={k} value={k} className="text-[11px] font-mono">
                {k}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </label>

      {(kind === "limit" || kind === "stop_limit") && (
        <MoneyField label="limit price (USD)" value={String(values.o_price ?? "")} onChange={(v) => set("o_price", v)} />
      )}
      {kind === "stop_market" || kind === "stop_limit" ? (
        <MoneyField label="trigger (USD)" value={String(values.o_trigger ?? "")} onChange={(v) => set("o_trigger", v)} />
      ) : null}
      {kind === "trailing_stop_market" || kind === "trailing_stop_limit" ? (
        <IntField label="offset_ticks" value={String(values.o_offset ?? "")} onChange={(v) => set("o_offset", v)} />
      ) : null}
      {kind === "trailing_stop_limit" && (
        <IntField label="limit_ticks" value={String(values.o_limitOffset ?? "")} onChange={(v) => set("o_limitOffset", v)} />
      )}

      <label className="flex flex-col gap-1">
        <span className="text-[9px] uppercase tracking-wider text-muted-foreground">tif.kind</span>
        <Select value={tif} onValueChange={(v) => set("o_tif", v)}>
          <SelectTrigger size="sm" className="h-8 w-full text-[11px] font-mono bg-transparent border-hairline">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {["gtc", "ioc", "fok", "gtd"].map((k) => (
              <SelectItem key={k} value={k} className="text-[11px] font-mono">
                {k}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </label>
      {tif === "gtd" && (
        <IntField label="gtd until (s from now)" value={String(values.o_gtd_s ?? "")} onChange={(v) => set("o_gtd_s", v)} />
      )}

      <span className="col-span-2 flex items-center gap-4 pt-0.5">
        <span className="flex items-center gap-2">
          <Switch checked={values.o_post === true} onCheckedChange={(c) => set("o_post", c)} aria-label="post only" />
          <span className="text-[9px] font-mono text-muted-foreground">post_only</span>
        </span>
        <span className="flex items-center gap-2">
          <Switch checked={values.o_ro === true} onCheckedChange={(c) => set("o_ro", c)} aria-label="reduce only" />
          <span className="text-[9px] font-mono text-muted-foreground">reduce_only</span>
        </span>
        <span className="ml-auto text-[9px] font-mono text-muted-foreground/50">tick {tick}</span>
      </span>
    </div>
  );
}

function MoneyField({ label, value, onChange }: { label: string; value: string; onChange: (v: string) => void }) {
  return (
    <label className="flex flex-col gap-1">
      <span className="text-[9px] uppercase tracking-wider text-muted-foreground">{label}</span>
      <Input
        value={value}
        onChange={(e) => onChange(e.target.value)}
        inputMode="decimal"
        className="h-8 text-[11px] font-mono bg-transparent border-hairline num"
      />
    </label>
  );
}

function IntField({ label, value, onChange }: { label: string; value: string; onChange: (v: string) => void }) {
  return (
    <label className="flex flex-col gap-1">
      <span className="text-[9px] uppercase tracking-wider text-muted-foreground">{label}</span>
      <Input
        value={value}
        onChange={(e) => onChange(e.target.value)}
        inputMode="numeric"
        className="h-8 text-[11px] font-mono bg-transparent border-hairline num"
      />
    </label>
  );
}
