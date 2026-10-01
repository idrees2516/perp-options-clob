/**
 * Market data + books — mirrors poc-orderbook `BookView` and poc-api session model.
 */

import type { MoneyMinor, PriceTicks, QtyLots, Symbol } from "./primitives";
import type { InstrumentKind } from "./instrument";

/** One price level: (price_ticks, lots). */
export type Level = { price_ticks: PriceTicks; lots: QtyLots };

/** L2 book snapshot: bids desc, asks asc. */
export interface BookSnapshot {
  seq: number;
  bids: Level[];
  asks: Level[];
}

/** L2 delta: levels REPLACE; 0 lots removes. seq must be last+1. */
export interface BookDelta {
  seq: number;
  bids: Level[];
  asks: Level[];
}

export type BookUpdate = BookSnapshot | BookDelta;

export function isSnapshot(u: BookUpdate): u is BookSnapshot {
  return (u as BookSnapshot).bids !== undefined && !("bids" in u) ? true : "seq" in u && !("isDelta" in u) ? true : false;
}

/** Best bid/ask. */
export type Bbo = { bid: Level | null; ask: Level | null };

export function bboOf(bids: Level[], asks: Level[]): Bbo {
  return { bid: bids.length ? bids[0] : null, ask: asks.length ? asks[0] : null };
}

/** Mid price in minor units; null for one-sided books. */
export function midMinor(bids: Level[], asks: Level[], tickMinor: bigint): MoneyMinor | null {
  if (!bids.length || !asks.length) return null;
  return (BigInt(bids[0].price_ticks) + BigInt(asks[0].price_ticks)) * tickMinor / 2n;
}

/** Engine `BookView`. */
export interface BookView {
  symbol: Symbol;
  best_bid_ticks: PriceTicks | null;
  best_ask_ticks: PriceTicks | null;
  open_orders: number;
  halted: boolean;
  kind: InstrumentKind;
}

/** Engine `MarketStateView` projection. */
export interface MarketStateView {
  books: BookView[];
  spots: { base: string; spot_quote_minor: MoneyMinor | null }[];
  insurance_balance: bigint;
}

/** Public trade print for the tape. */
export interface TradePrint {
  seq: number;
  ts: number;
  symbol: Symbol;
  price_ticks: PriceTicks;
  qty_lots: QtyLots;
  maker_side: "bid" | "ask";
  notional_quote_minor: MoneyMinor;
}

/** Candle for charts (client aggregation of prints). */
export interface Candle {
  ts: number;
  open: number;
  high: number;
  low: number;
  close: number;
  volume: number;
}

/** Oracle provider status for the defense panel. */
export interface OracleProviderState {
  provider: string;
  base_symbol: string;
  last_price_quote_minor: MoneyMinor | null;
  last_ts: number;
  quarantined: boolean;
  /** Deviation from median in bps at last observation. */
  deviation_bps: number | null;
}

export interface OracleState {
  base_symbol: string;
  providers: OracleProviderState[];
  median_quote_minor: MoneyMinor | null;
  quorum: number;
  min_quorum: number;
  halted: boolean;
  last_update: number;
}
