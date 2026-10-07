/**
 * Engine events — all variants of `Event` (crates/engine/src/event.rs).
 * The journal is the state machine; the frontend renders it directly.
 */

import type {
  MoneyMinor,
  Permille,
  QtyLots,
  Side,
  SignedMoneyMinor,
  SubaccountId,
  Symbol,
  TimestampMs,
} from "./primitives";
import type { Instrument } from "./instrument";
import type { Order, OrderRequest, Rejection } from "./order";

/** TradeExecuted payload. */
export interface Trade {
  seq: number;
  symbol: Symbol;
  taker_order_id: number;
  maker_order_id: number;
  taker_subaccount: SubaccountId;
  maker_subaccount: SubaccountId;
  maker_side: Side;
  price_ticks: number;
  qty_lots: QtyLots;
  notional_quote_minor: MoneyMinor;
  taker_fee_quote_minor: SignedMoneyMinor;
  maker_fee_quote_minor: SignedMoneyMinor;
  ts: TimestampMs;
}

export interface OrderRejected {
  request: OrderRequest;
  reason: Rejection;
  order_id: number;
}

export interface FundingSettled {
  symbol: Symbol;
  rate_bps: number;
  ts: TimestampMs;
}

export interface FundingPaid {
  subaccount: SubaccountId;
  symbol: Symbol;
  credit_quote_minor: SignedMoneyMinor;
}

export interface OptionSettled {
  subaccount: SubaccountId;
  symbol: Symbol;
  signed_lots: number;
  settlement_quote_minor: MoneyMinor;
  payout_quote_minor: SignedMoneyMinor;
}

export interface ExerciseAssignment {
  subaccount: SubaccountId;
  lots: QtyLots;
  charge_quote_minor: MoneyMinor;
}

export interface OptionExercised {
  request_id: number;
  subaccount: SubaccountId;
  symbol: Symbol;
  requested_lots: QtyLots;
  settled_lots: QtyLots;
  settlement_quote_minor: MoneyMinor;
  intrinsic_per_lot_quote_minor: MoneyMinor;
  gross_payout_quote_minor: SignedMoneyMinor;
  exercise_fee_quote_minor: MoneyMinor;
  assignments: ExerciseAssignment[];
  ts: TimestampMs;
}

export interface LiquidationExecuted {
  subaccount: SubaccountId;
  symbol: Symbol;
  lots: QtyLots;
  price_quote_minor: MoneyMinor;
  to_insurance: boolean;
  penalty_quote_minor: MoneyMinor;
  absorbed_quote_minor: MoneyMinor;
  closing_side_is_ask: boolean;
}

export interface AdlExecuted {
  liquidated_subaccount: SubaccountId;
  counterparty_subaccount: SubaccountId;
  symbol: Symbol;
  lots: QtyLots;
  price_quote_minor: MoneyMinor;
  closing_side_is_ask: boolean;
}

export interface RewardPaid {
  subaccount: SubaccountId;
  amount_quote_minor: MoneyMinor;
}

export interface LiquidityObservation {
  subaccount: SubaccountId;
  size_lots: QtyLots;
  spread_bps: number;
  two_sided: boolean;
  side: Side;
}

export interface OrderAmended {
  order_id: number;
  subaccount: SubaccountId;
  symbol: Symbol;
  new_open_lots: QtyLots;
  ts: TimestampMs;
}

export interface TwapParent {
  parent_id: number;
  subaccount: SubaccountId;
  symbol: Symbol;
  side: Side;
  total_lots: QtyLots;
  slices: number;
  slice_interval_ms: number;
  limit_ticks: number | null;
  next_slice_ts: TimestampMs;
  slices_placed: number;
  lots_placed: QtyLots;
  opened_ts: TimestampMs;
}

export interface VaultEpoch {
  vault_id: number;
  epoch: number;
  nav_per_share_quote_minor: MoneyMinor;
  subscribed_shares: bigint;
  subscribed_quote_minor: MoneyMinor;
  redeemed_shares: bigint;
  redeemed_quote_minor: MoneyMinor;
  insurance_credit_quote_minor: MoneyMinor;
  nav_after_quote_minor: MoneyMinor;
  flows: { subaccount: SubaccountId; amount: SignedMoneyMinor }[];
  ts: TimestampMs;
}

export interface CollateralMoved {
  subaccount: SubaccountId;
  currency: string;
  amount_minor: SignedMoneyMinor;
  ts: TimestampMs;
}

export interface CollateralConverted {
  subaccount: SubaccountId;
  from: string;
  to: string;
  from_amount_minor: MoneyMinor;
  to_amount_minor: MoneyMinor;
  rate_quote_minor_per_unit: MoneyMinor;
  ts: TimestampMs;
}

export interface SurfaceObservation {
  symbol: Symbol;
  iv_bps: number;
  weight: number;
  ts: TimestampMs;
}

export type Event =
  | { type: "deposit"; subaccount: SubaccountId; amount_quote_minor: MoneyMinor; ts: TimestampMs }
  | { type: "withdrawal"; subaccount: SubaccountId; amount_quote_minor: MoneyMinor; ts: TimestampMs }
  | {
      type: "withdraw_rejected";
      subaccount: SubaccountId;
      requested: MoneyMinor;
      reason: string;
      ts: TimestampMs;
    }
  | {
      type: "provider_observed";
      base_symbol: string;
      provider: string;
      ts: TimestampMs;
      price_quote_minor: MoneyMinor;
    }
  | { type: "market_listed"; instrument: Instrument; anchor_iv_bps: number | null }
  | { type: "clock_advanced"; now: TimestampMs }
  | { type: "order_resting"; order: Order; margin_reserved_quote_minor: MoneyMinor }
  | {
      type: "order_closed";
      order_id: number;
      subaccount: SubaccountId;
      symbol: Symbol;
      order: Order;
      reason: "filled" | "canceled" | "expired" | "ioc_remainder" | "oco_sibling";
    }
  | { type: "order_rejection"; payload: OrderRejected }
  | { type: "trade_executed"; payload: Trade }
  | { type: "stp_cancels"; maker_ids: number[]; ts: TimestampMs }
  | { type: "funding"; payload: FundingSettled }
  | { type: "funding_flow"; payload: FundingPaid }
  | { type: "option_expiry"; payload: OptionSettled }
  | {
      type: "exercise_queued";
      request_id: number;
      subaccount: SubaccountId;
      symbol: Symbol;
      lots: QtyLots;
      requested_at: TimestampMs;
      settle_at: TimestampMs;
    }
  | { type: "option_exercised"; payload: OptionExercised }
  | {
      type: "exercise_rejected";
      subaccount: SubaccountId;
      symbol: Symbol;
      requested_lots: QtyLots;
      reason: string;
      ts: TimestampMs;
    }
  | { type: "exercise_deferred"; request_id: number; new_settle_at: TimestampMs }
  | { type: "option_delisted"; symbol: Symbol }
  | { type: "liquidity_scored"; observations: LiquidityObservation[] }
  | { type: "reward"; payload: RewardPaid }
  | { type: "rewards_settled" }
  | { type: "liquidation"; payload: LiquidationExecuted }
  | { type: "adl"; payload: AdlExecuted }
  | { type: "market_halted"; base_symbol: string; ts: TimestampMs }
  | { type: "market_resumed"; base_symbol: string; ts: TimestampMs }
  | {
      type: "rfq_created";
      taker: SubaccountId;
      legs: { symbol: Symbol; side: Side; qty_lots: QtyLots }[];
      counterparties: SubaccountId[];
      min_total_cost_quote_minor: MoneyMinor | null;
      max_total_cost_quote_minor: MoneyMinor | null;
      ttl_ms: number;
      ts: TimestampMs;
    }
  | {
      type: "rfq_quoted";
      rfq_id: number;
      maker: SubaccountId;
      leg_prices_ticks: number[];
      ttl_ms: number;
      ts: TimestampMs;
    }
  | { type: "rfq_rejected"; subaccount: SubaccountId; reason: string; ts: TimestampMs }
  | {
      type: "rfq_settled";
      rfq_id: number;
      quote_id: number;
      taker: SubaccountId;
      maker: SubaccountId;
      trades: Trade[];
      taker_fees_quote_minor: SignedMoneyMinor[];
    }
  | {
      type: "rfq_closed";
      rfq_id: number;
      quote_id: number | null;
      reason: "cancelled" | "expired" | "filled";
    }
  | { type: "cod_changed"; subaccount: SubaccountId; enabled: boolean; ts: TimestampMs }
  | {
      type: "block_registered";
      taker: SubaccountId;
      maker: SubaccountId;
      legs: { symbol: Symbol; side: Side; qty_lots: QtyLots; price_ticks: number }[];
      total_notional_quote_minor: MoneyMinor;
      taker_fees_quote_minor: SignedMoneyMinor[];
      broadcast_ts: TimestampMs;
    }
  | { type: "block_printed"; block_id: number }
  | {
      type: "transfer_executed";
      from: SubaccountId;
      to: SubaccountId;
      amount_quote_minor: MoneyMinor;
      ts: TimestampMs;
    }
  | {
      type: "transfer_rejected";
      from: SubaccountId;
      to: SubaccountId;
      requested: MoneyMinor;
      reason: string;
      ts: TimestampMs;
    }
  | {
      type: "mmp_configured";
      subaccount: SubaccountId;
      base_symbol: string;
      interval_ms: number;
      frozen_time_ms: number;
      amount_limit_lots: QtyLots;
      delta_limit_lots: QtyLots;
    }
  | { type: "mmp_tripped"; subaccount: SubaccountId; base_symbol: string; ts: TimestampMs }
  | { type: "session_disconnected"; subaccount: SubaccountId; canceled_orders: number[]; ts: TimestampMs }
  | { type: "surface_observed"; payload: SurfaceObservation }
  | { type: "surface_swept"; now: TimestampMs }
  | {
      type: "breaker_tripped";
      kind: "price-dislocation" | "cascade-velocity";
      symbol: Symbol;
      ts: TimestampMs;
    }
  | { type: "breaker_released"; kind: "price-dislocation" | "cascade-velocity"; symbol: Symbol; ts: TimestampMs }
  | { type: "order_amended"; payload: OrderAmended }
  | {
      type: "trailing_updated";
      order_id: number;
      subaccount: SubaccountId;
      symbol: Symbol;
      extreme_quote_minor: MoneyMinor;
      ts: TimestampMs;
    }
  | { type: "auction_opened"; symbol: Symbol; uncross_at: TimestampMs; ts: TimestampMs }
  | {
      type: "auction_uncrossed";
      symbol: Symbol;
      clearing_price_ticks: number | null;
      matched_lots: QtyLots;
      taker_reductions: { order_id: number; lots: QtyLots }[];
      ts: TimestampMs;
    }
  | { type: "collateral_moved"; payload: CollateralMoved }
  | {
      type: "collateral_rejected";
      subaccount: SubaccountId;
      currency: string;
      requested_minor: MoneyMinor;
      reason: string;
      ts: TimestampMs;
    }
  | { type: "collateral_conversion"; payload: CollateralConverted }
  | {
      type: "position_migrated";
      subaccount: SubaccountId;
      from_symbol: Symbol;
      to_symbol: Symbol;
      signed_lots: number;
      close_price_quote_minor: MoneyMinor;
      open_price_quote_minor: MoneyMinor;
      ts: TimestampMs;
    }
  | { type: "oco_linked"; group: number; first: number; second: number; ts: TimestampMs }
  | { type: "twap_opened"; payload: TwapParent }
  | {
      type: "twap_sliced";
      parent_id: number;
      request: OrderRequest;
      slice_index: number;
      ts: TimestampMs;
    }
  | {
      type: "twap_closed";
      parent_id: number;
      subaccount: SubaccountId;
      reason: "completed" | "canceled";
      placed_lots: QtyLots;
      ts: TimestampMs;
    }
  | { type: "vol_index_published"; base_symbol: string; index_permille: number; ts: TimestampMs }
  | {
      type: "collateral_interest_accrued";
      subaccount: SubaccountId;
      currency: string;
      amount_minor: MoneyMinor;
      quote_value_minor: MoneyMinor;
      ts: TimestampMs;
    }
  | {
      type: "insurance_marked";
      symbol: Symbol;
      signed_lots: number;
      mark_quote_minor: MoneyMinor;
      pnl_quote_minor: SignedMoneyMinor;
      ts: TimestampMs;
    }
  | { type: "insurance_rebalanced"; symbol: Symbol; lots: QtyLots; proceeds_quote_minor: MoneyMinor; ts: TimestampMs }
  | { type: "vault_epoch_settled"; payload: VaultEpoch }
  | { type: "vault_opened"; vault_id: number; revenue_share_bps: number; ts: TimestampMs }
  | {
      type: "vault_queued";
      vault_id: number;
      subaccount: SubaccountId;
      is_subscribe: boolean;
      amount: MoneyMinor;
      ts: TimestampMs;
    }
  | { type: "mm_enrolled"; subaccount: SubaccountId; ts: TimestampMs }
  | {
      type: "mm_tier_adjusted";
      subaccount: SubaccountId;
      tier: string | null;
      fee_discount_bps: number;
      uptime_permille: Permille;
      ticks: number;
      ts: TimestampMs;
    }
  | { type: "quote_interest_accrued"; subaccount: SubaccountId; amount_quote_minor: MoneyMinor; ts: TimestampMs };

export type EventType = Event["type"];

/** Every event type — journal filter UI iterates this. */
export const EVENT_TYPES = [
  "deposit",
  "withdrawal",
  "withdraw_rejected",
  "provider_observed",
  "market_listed",
  "clock_advanced",
  "order_resting",
  "order_closed",
  "order_rejection",
  "trade_executed",
  "stp_cancels",
  "funding",
  "funding_flow",
  "option_expiry",
  "exercise_queued",
  "option_exercised",
  "exercise_rejected",
  "exercise_deferred",
  "option_delisted",
  "liquidity_scored",
  "reward",
  "rewards_settled",
  "liquidation",
  "adl",
  "market_halted",
  "market_resumed",
  "rfq_created",
  "rfq_quoted",
  "rfq_settled",
  "rfq_rejected",
  "rfq_closed",
  "cod_changed",
  "block_registered",
  "block_printed",
  "transfer_executed",
  "transfer_rejected",
  "mmp_configured",
  "mmp_tripped",
  "session_disconnected",
  "surface_observed",
  "surface_swept",
  "breaker_tripped",
  "breaker_released",
  "order_amended",
  "trailing_updated",
  "auction_opened",
  "auction_uncrossed",
  "collateral_moved",
  "collateral_rejected",
  "collateral_conversion",
  "position_migrated",
  "oco_linked",
  "twap_opened",
  "twap_sliced",
  "twap_closed",
  "vol_index_published",
  "collateral_interest_accrued",
  "insurance_marked",
  "insurance_rebalanced",
  "vault_epoch_settled",
  "vault_opened",
  "vault_queued",
  "mm_enrolled",
  "mm_tier_adjusted",
  "quote_interest_accrued",
] as const satisfies readonly EventType[];

/** Journal entry as consumed by the frontend. */
export interface JournalEntry {
  /** Monotonic sequence in the journal. */
  seq: number;
  ts: TimestampMs;
  event: Event;
}
