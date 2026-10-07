/**
 * Order model — mirrors poc-core `OrderRequest` / `Order` and engine command payloads.
 */

import type {
  OrderId,
  OrderType,
  PriceTicks,
  QtyLots,
  SelfTradePrevention,
  Side,
  SubaccountId,
  Symbol,
  TimeInForce,
  TimestampMs,
} from "./primitives";

/** A placeable order request — the engine `OrderRequest` verbatim. */
export interface OrderRequest {
  subaccount: SubaccountId;
  symbol: Symbol;
  side: Side;
  order_type: OrderType;
  /** Limit price in ticks (required for limit-class orders). */
  price_ticks: PriceTicks | null;
  qty_lots: QtyLots;
  tif: TimeInForce;
  post_only: boolean;
  reduce_only: boolean;
  stp: SelfTradePrevention;
  /** Iceberg display size in lots (G-06); null = full display. */
  display_lots: QtyLots | null;
  oco_group: number | null;
  client_ts: TimestampMs;
}

/** Convenience builders mirroring the Rust helpers. */
export function limitOrder(
  subaccount: SubaccountId,
  symbol: Symbol,
  side: Side,
  priceTicks: PriceTicks,
  qtyLots: QtyLots,
  extra: Partial<OrderRequest> = {},
): OrderRequest {
  return {
    subaccount,
    symbol,
    side,
    order_type: { kind: "limit" },
    price_ticks: priceTicks,
    qty_lots: qtyLots,
    tif: { kind: "gtc" },
    post_only: false,
    reduce_only: false,
    stp: "cancel_newest",
    display_lots: null,
    oco_group: null,
    client_ts: 0,
    ...extra,
  };
}

export function marketOrder(
  subaccount: SubaccountId,
  symbol: Symbol,
  side: Side,
  qtyLots: QtyLots,
  extra: Partial<OrderRequest> = {},
): OrderRequest {
  return {
    subaccount,
    symbol,
    side,
    order_type: { kind: "market" },
    price_ticks: null,
    qty_lots: qtyLots,
    tif: { kind: "ioc" },
    post_only: false,
    reduce_only: false,
    stp: "cancel_newest",
    display_lots: null,
    oco_group: null,
    client_ts: 0,
    ...extra,
  };
}

/** Full resting/live order state as journaled inside events. */
export interface Order {
  id: OrderId;
  subaccount: SubaccountId;
  symbol: Symbol;
  side: Side;
  order_type: OrderType;
  price_ticks: PriceTicks | null;
  qty_lots: QtyLots;
  filled_lots: QtyLots;
  tif: TimeInForce;
  post_only: boolean;
  reduce_only: boolean;
  stp: SelfTradePrevention;
  display_lots: QtyLots | null;
  /** Trailing-stop extreme (mark high/low since placement). */
  trailing_extreme_quote_minor: bigint | null;
  oco_group: number | null;
  client_ts: TimestampMs;
  engine_ts: TimestampMs;
  /** Client-side bookkeeping (not journaled on the Rust side). */
  state: OrderStateLite;
}

/** Lightweight client-side lifecycle used by UI lists. */
export type OrderStateLite = "pending" | "open" | "filled" | "canceled" | "rejected" | "expired";

export function orderOpenLots(o: Pick<Order, "qty_lots" | "filled_lots">): QtyLots {
  return Math.max(0, o.qty_lots - o.filled_lots);
}

export function orderIsTerminal(o: Pick<Order, "state">): boolean {
  return o.state === "filled" || o.state === "canceled" || o.state === "rejected" || o.state === "expired";
}

/** Reason a pre-trade risk gate rejected an order (poc-risk `Rejection`). */
export type Rejection =
  | { kind: "unknown_instrument" }
  | { kind: "unknown_account" }
  | { kind: "invalid_order"; reason: string }
  | { kind: "market_halted" }
  | { kind: "outside_price_band"; price_quote_minor: bigint; mark_quote_minor: bigint }
  | { kind: "post_only_would_cross" }
  | { kind: "reduce_only_would_increase" }
  | { kind: "position_limit_exceeded"; projected_lots: number; max_lots: number }
  | { kind: "too_many_open_orders"; current: number; max: number }
  | { kind: "insufficient_margin"; shortfall: bigint; equity_after: bigint }
  | { kind: "missing_mark" }
  | { kind: "greeks_limit_exceeded"; what: string; would_be: number; cap: number };

/** RFQ leg (command side). */
export interface RfqLegCommand {
  symbol: Symbol;
  side: Side;
  qty_lots: QtyLots;
}
